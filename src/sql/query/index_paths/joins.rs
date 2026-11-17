//! PostgreSQL 18's join search over plain relations: `standard_join_search`
//! and `join_search_one_level` (`joinrels.c`), `make_join_rel` and
//! `add_paths_to_joinrel` with the merge, nested-loop and hash path
//! generators (`joinpath.c`), the join costs of `costsize.c`
//! (`initial_cost_nestloop` .. `final_cost_hashjoin`, `cost_material`,
//! `cost_rescan`, `calc_joinrel_size_estimate`) and `add_path` and
//! `set_cheapest` (`pathnode.c`).
//!
//! The search reads its relations, equivalence classes and join clauses as
//! numbers and indexes; [`JoinStatistics`] answers the selectivity questions
//! (`eqjoinsel`, `estimate_hash_bucket_stats`, `mergejoinscansel`) from the
//! relations' statistics.
//! <https://github.com/postgres/postgres/blob/REL_18_6/src/backend/optimizer/path/joinrels.c>
//! <https://github.com/postgres/postgres/blob/REL_18_6/src/backend/optimizer/path/joinpath.c>
//! <https://github.com/postgres/postgres/blob/REL_18_6/src/backend/optimizer/path/costsize.c>

use crate::mem::arena::{Arena, ArenaList};
use crate::sql::eval::SqlError;

use super::{Costs, clamp_row_est};

/// Relations a join search plans at most: `join_collapse_limit`'s and
/// `from_collapse_limit`'s default, below which PostgreSQL searches one
/// flat problem.
pub(crate) const MAX_JOIN_RELS: usize = 8;
/// Members per equivalence class and clauses the search holds.
pub(crate) const MAX_MEMBERS: usize = 16;
pub(crate) const MAX_CLAUSES: usize = 64;
/// Join clauses of one join, and sort keys of one path.
pub(crate) const MAX_JOIN_CLAUSES: usize = 16;
pub(crate) const MAX_KEYS: usize = 8;
/// Paths kept per relation by `add_path`.
const MAX_REL_PATHS: usize = 16;

/// A set of base relations, by bit.
pub(crate) type Relids = u32;

/// `STD_FUZZ_FACTOR`.
const FUZZ: f64 = 1.01;
/// `BLCKSZ`.
const BLOCK: f64 = 8192.0;

/// A column of a base relation that an equivalence class equates.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Member {
    pub(crate) rel: u8,
    pub(crate) column: u16,
}

/// An equivalence class of columns compared by one btree and hash
/// operator family. A class holding a constant joins nothing: its members
/// are each restricted to the constant instead.
#[derive(Clone, Copy, Debug)]
pub(crate) struct EquivalenceClass {
    pub(crate) members: [Member; MAX_MEMBERS],
    pub(crate) count: u8,
    pub(crate) has_const: bool,
}

impl EquivalenceClass {
    pub(crate) fn members(&self) -> &[Member] {
        &self.members[..usize::from(self.count)]
    }

    fn relids(&self) -> Relids {
        self.members()
            .iter()
            .fold(0, |relids, member| relids | 1 << member.rel)
    }
}

/// How a join clause compares its sides.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ClauseForm {
    /// `left = right` of two members of an equivalence class: mergeable
    /// and hashable.
    Equality { class: u8, left: u8, right: u8 },
    /// Any other condition reading two or more relations.
    Other,
}

/// A join clause (`RestrictInfo`).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Clause {
    pub(crate) form: ClauseForm,
    pub(crate) relids: Relids,
    /// `cost_qual_eval`'s per-tuple cost.
    pub(crate) cost: f64,
    /// Its selectivity for an inner join, once computed.
    pub(crate) selectivity: Option<f64>,
    /// The caller's clause this one is, or `None` for one an equivalence
    /// class implied.
    pub(crate) source: Option<u16>,
}

/// A base relation's size and the paths `set_plain_rel_pathlist` submits
/// to `add_path`, in that order.
#[derive(Clone, Copy, Debug)]
pub(crate) struct BaseRel<'p> {
    pub(crate) rows: f64,
    pub(crate) width: f64,
    pub(crate) paths: &'p [BasePath],
    /// The key columns of each unique, immediately enforced, whole-table
    /// index whose keys are plain columns of the default operator class.
    pub(crate) unique: &'p [u64],
    /// The columns its restrictions equate with a constant.
    pub(crate) fixed: u64,
}

/// One path of a base relation.
#[derive(Clone, Copy, Debug)]
pub(crate) struct BasePath {
    pub(crate) startup: f64,
    pub(crate) total: f64,
    pub(crate) rows: f64,
    pub(crate) disabled: u32,
    pub(crate) keys: Keys,
    /// The relations whose values it reads (`PATH_REQ_OUTER`).
    pub(crate) param: Relids,
    /// The join clauses a parameterized path enforces (`ppi_clauses`).
    pub(crate) enforced: ClauseList,
    /// An index scan, which supports mark and restore.
    pub(crate) mark_restore: bool,
    /// A parameterized index scan (or bitmap of one) whose index conditions
    /// apply every join clause it enforces (`has_indexed_join_quals`).
    pub(crate) indexed_join_quals: bool,
}

/// A column of a base relation that something above its scan reads.
#[derive(Clone, Copy, Debug)]
pub(crate) struct NeededColumn {
    pub(crate) rel: u8,
    pub(crate) column: u16,
    pub(crate) width: f64,
    /// The relations whose joins read it (`attr_needed`).
    pub(crate) needed_by: Relids,
    /// The query's output reads it.
    pub(crate) output: bool,
}

/// The selectivity questions a join search asks of the statistics.
pub(crate) trait JoinStatistics {
    /// `eqjoinsel` of `left = right` for an inner join.
    fn equality(&self, left: Member, right: Member) -> Result<f64, SqlError>;
    /// `estimate_num_groups` of the columns `members` over `calls` rows;
    /// `calls` when a column's distinct count is a default.
    fn groups(&self, members: &[Member], calls: f64) -> Result<f64, SqlError>;
    /// `estimate_hash_bucket_stats` of `member` over `buckets`: its most
    /// common value's frequency and its bucket size fraction.
    fn bucket(&self, member: Member, buckets: f64) -> Result<(f64, f64), SqlError>;
    /// `mergejoinscansel` of `left = right` merged ascending:
    /// `(leftstart, leftend, rightstart, rightend)`.
    /// `None` when PostgreSQL's estimate is outside the model.
    fn merge_range(&self, left: Member, right: Member) -> Result<Option<[f64; 4]>, SqlError>;
}

/// The planner settings a join search reads.
#[derive(Clone, Copy, Debug)]
pub(crate) struct JoinSettings {
    pub(crate) costs: Costs,
    pub(crate) work_mem_bytes: f64,
    pub(crate) hash_mem_bytes: f64,
    pub(crate) nestloop: bool,
    pub(crate) hashjoin: bool,
    pub(crate) mergejoin: bool,
    pub(crate) material: bool,
    pub(crate) sort: bool,
    pub(crate) memoize: bool,
}

/// An ordered list of clause numbers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct ClauseList {
    ids: [u8; MAX_JOIN_CLAUSES],
    len: u8,
}

impl ClauseList {
    pub(crate) const EMPTY: Self = Self {
        ids: [0; MAX_JOIN_CLAUSES],
        len: 0,
    };

    pub(crate) fn as_slice(&self) -> &[u8] {
        &self.ids[..usize::from(self.len)]
    }

    fn push(&mut self, id: u8) -> Option<()> {
        *self.ids.get_mut(usize::from(self.len))? = id;
        self.len += 1;
        Some(())
    }

    fn contains(&self, id: u8) -> bool {
        self.as_slice().contains(&id)
    }

    /// Appends clause `id`, or `None` when the list is full.
    pub(crate) fn push_id(&mut self, id: u8) -> Option<()> {
        self.push(id)
    }
}

/// A path's sort order: equivalence classes, each ascending with NULLs
/// last, as merge joins sort.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Keys {
    classes: [u8; MAX_KEYS],
    len: u8,
}

impl Keys {
    pub(crate) const NONE: Self = Self {
        classes: [0; MAX_KEYS],
        len: 0,
    };

    pub(crate) fn as_slice(&self) -> &[u8] {
        &self.classes[..usize::from(self.len)]
    }

    fn push(&mut self, class: u8) -> Option<()> {
        *self.classes.get_mut(usize::from(self.len))? = class;
        self.len += 1;
        Some(())
    }

    /// Appends `class`, or `None` when the keys are full.
    pub(crate) fn push_class(&mut self, class: u8) -> Option<()> {
        self.push(class)
    }

    /// Keys of the classes `classes`.
    pub(crate) fn of(classes: &[u8]) -> Self {
        let mut keys = Self::NONE;
        for &class in classes {
            let _ = keys.push(class);
        }
        keys
    }

    fn truncated(mut self, len: usize) -> Self {
        self.len = self.len.min(len as u8);
        self
    }

    /// `pathkeys_contained_in(self, other)`.
    fn contained_in(&self, other: &Self) -> bool {
        self.len <= other.len && other.as_slice().starts_with(self.as_slice())
    }

    /// The leading keys of `self` that `other` starts with.
    fn common_prefix(&self, other: &Self) -> usize {
        self.as_slice()
            .iter()
            .zip(other.as_slice())
            .take_while(|(left, right)| left == right)
            .count()
    }
}

/// What a path does.
#[derive(Clone, Copy, Debug)]
pub(crate) enum PathKind {
    Scan {
        rel: u8,
        /// Its position in [`BaseRel::paths`].
        path: u16,
    },
    Material {
        input: u32,
    },
    NestLoop {
        outer: u32,
        inner: u32,
        clauses: ClauseList,
        /// The inner relation is unique for the join's clauses: the loop
        /// moves to the next outer row at the first match.
        inner_unique: bool,
    },
    /// A cache of a parameterized inner path's rows by its parameters'
    /// values (`Memoize`): `keys` are the outer columns, as class members.
    Memoize {
        input: u32,
        keys: [Member; MAX_KEYS],
        key_count: u8,
        /// `singlerow`: the join's inner relation is unique, so an entry
        /// is complete at its first row.
        single_row: bool,
    },
    HashJoin {
        outer: u32,
        inner: u32,
        clauses: ClauseList,
        hash: ClauseList,
        buckets: u32,
        batches: u32,
    },
    MergeJoin {
        outer: u32,
        inner: u32,
        clauses: ClauseList,
        merge: ClauseList,
        outer_sort: Keys,
        inner_sort: Keys,
        materialize_inner: bool,
        /// The inner relation is unique: each outer row stops at its first
        /// match, and with no other clause the inner input is never marked.
        inner_unique: bool,
    },
}

/// A path of a base or join relation.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Path {
    pub(crate) kind: PathKind,
    pub(crate) relids: Relids,
    pub(crate) rows: f64,
    pub(crate) width: f64,
    pub(crate) startup: f64,
    pub(crate) total: f64,
    pub(crate) disabled: u32,
    pub(crate) keys: Keys,
    /// The relations outside it whose values it reads (`PATH_REQ_OUTER`).
    pub(crate) param: Relids,
}

/// A relation the search built: its paths and, once its level is done,
/// its cheapest ones.
#[derive(Clone, Copy)]
struct Rel {
    rows: f64,
    width: f64,
    paths: [u32; MAX_REL_PATHS],
    count: u8,
    cheapest_total: u32,
    cheapest_startup: u32,
    /// `cheapest_parameterized_paths`: the cheapest total path, then each
    /// parameterized path.
    parameterized: [u32; MAX_REL_PATHS],
    parameterized_count: u8,
    /// The pair that built a join relation, whose target lists its own
    /// concatenates (`build_joinrel_tlist`).
    first: (Relids, Relids),
}

/// The chosen plan: every path the search built, and the cheapest of the
/// relation joining them all.
pub(crate) struct JoinPlan<'a> {
    pub(crate) paths: &'a [Path],
    pub(crate) clauses: &'a [Clause],
    pub(crate) classes: &'a [EquivalenceClass],
    pub(crate) top: u32,
    pub(crate) settings: JoinSettings,
    /// For each join relation, by relids, the pair that built it.
    pub(crate) firsts: &'a [(Relids, Relids)],
    pub(crate) needed: &'a [NeededColumn],
}

impl JoinPlan<'_> {
    pub(crate) fn path(&self, id: u32) -> &Path {
        &self.paths[id as usize]
    }

    /// The columns of `relids`' target list in its order: each relation's
    /// needed columns that a relation outside `relids` or the output reads,
    /// a join relation's as its building pair's outer then inner lists.
    pub(crate) fn target_list(&self, relids: Relids, visit: &mut dyn FnMut(&NeededColumn)) {
        self.target_list_of(relids, relids, visit);
    }

    fn target_list_of(&self, part: Relids, above: Relids, visit: &mut dyn FnMut(&NeededColumn)) {
        let (outer, inner) = self.firsts[part as usize];
        if inner == 0 {
            for column in self.needed {
                if part & 1 << column.rel != 0 && (column.output || column.needed_by & !above != 0)
                {
                    visit(column);
                }
            }
            return;
        }
        self.target_list_of(outer, above, visit);
        self.target_list_of(inner, above, visit);
    }
}

/// A problem outside the model; the text names the shape.
pub(crate) type Outside = &'static str;

/// A foreign key of one relation referencing another whose every column
/// pair an equivalence class equates (`match_foreign_keys_to_quals`).
#[derive(Clone, Copy, Debug)]
pub(crate) struct ForeignKeyJoin {
    pub(crate) referencing: u8,
    pub(crate) referenced: u8,
    /// Each key column pair's class.
    pub(crate) classes: [u8; MAX_KEYS],
    pub(crate) count: u8,
    /// The referenced table's rows (`ref_rel->tuples`, at least one).
    pub(crate) referenced_tuples: f64,
}

/// The join problem.
pub(crate) struct JoinProblem<'p> {
    pub(crate) rels: &'p [BaseRel<'p>],
    pub(crate) classes: &'p [EquivalenceClass],
    /// The query's join clauses in the order PostgreSQL distributes them,
    /// equalities of class members included, then the clauses its base
    /// relations' index paths derived (`source` `None`).
    pub(crate) clauses: &'p [Clause],
    pub(crate) needed: &'p [NeededColumn],
    pub(crate) settings: JoinSettings,
    /// The query's foreign keys, as `root->fkey_list` holds them.
    pub(crate) foreign_keys: &'p [ForeignKeyJoin],
}

struct Search<'p, 'a, 's> {
    problem: &'p JoinProblem<'p>,
    statistics: &'s dyn JoinStatistics,
    arena: &'a Arena,
    clauses: ArenaList<'a, Clause>,
    paths: ArenaList<'a, Path>,
    /// Relations by their relids; `None` until built.
    rels: &'a mut [Option<Rel>],
    /// The joinrels of each level in the order they were built.
    levels: [ArenaList<'a, Relids>; MAX_JOIN_RELS + 1],
    /// A shape outside the model met while generating paths.
    outside: Option<Outside>,
    /// Each Memoize path's `cost_memoize_rescan`.
    memoize_rescans: ArenaList<'a, (u32, f64, f64)>,
    /// Each parameterized join's `ppi_rows`, by its relations and
    /// parameterization.
    param_rows: ArenaList<'a, (Relids, Relids, f64)>,
}

/// Plans the join of `problem`'s relations as `standard_join_search` does.
pub(crate) fn search<'a>(
    problem: &JoinProblem<'_>,
    statistics: &dyn JoinStatistics,
    arena: &'a Arena,
) -> Result<Result<JoinPlan<'a>, Outside>, SqlError> {
    let n = problem.rels.len();
    if !(2..=MAX_JOIN_RELS).contains(&n) {
        return Ok(Err("a join of more relations than join_collapse_limit"));
    }
    let full = arena_slice(arena, 1usize << n, |_| None::<Rel>)?;
    let mut search = Search {
        problem,
        statistics,
        arena,
        clauses: ArenaList::new(arena),
        paths: ArenaList::new(arena),
        rels: full,
        levels: core::array::from_fn(|_| ArenaList::new(arena)),
        outside: None,
        memoize_rescans: ArenaList::new(arena),
        param_rows: ArenaList::new(arena),
    };
    for clause in problem.clauses {
        search.clauses.push(*clause).map_err(|_| arena_full())?;
    }
    for (index, rel) in problem.rels.iter().enumerate() {
        let relids = 1 << index;
        search.rels[relids as usize] = Some(Rel {
            rows: rel.rows,
            width: rel.width,
            paths: [0; MAX_REL_PATHS],
            count: 0,
            cheapest_total: 0,
            cheapest_startup: 0,
            parameterized: [0; MAX_REL_PATHS],
            parameterized_count: 0,
            first: (relids, 0),
        });
        for (at, base) in rel.paths.iter().enumerate() {
            search.add_path(
                relids,
                Path {
                    kind: PathKind::Scan {
                        rel: index as u8,
                        path: at as u16,
                    },
                    relids,
                    rows: base.rows,
                    width: rel.width,
                    startup: base.startup,
                    total: base.total,
                    disabled: base.disabled,
                    keys: base.keys,
                    param: base.param,
                },
            )?;
        }
        search.set_cheapest(relids);
        search.levels[1].push(relids).map_err(|_| arena_full())?;
    }
    for level in 2..=n {
        if let Err(outside) = search.one_level(level)? {
            return Ok(Err(outside));
        }
        if let Some(outside) = search.outside {
            return Ok(Err(outside));
        }
        for at in 0..search.levels[level].len() {
            let relids = search.levels[level].borrowed_slice()[at];
            search.set_cheapest(relids);
        }
    }
    let all = ((1u32 << n) - 1) as usize;
    let Some(top) = search.rels[all] else {
        return Ok(Err("a join the search does not reach"));
    };
    Ok(Ok(JoinPlan {
        paths: search.paths.as_slice(),
        clauses: search.clauses.as_slice(),
        classes: arena
            .alloc_slice_copy(problem.classes)
            .map_err(|_| arena_full())?,
        top: top.cheapest_total,
        settings: problem.settings,
        firsts: arena
            .alloc_slice_with(search.rels.len(), |relids| {
                search.rels[relids].map_or((0, 0), |rel| rel.first)
            })
            .map_err(|_| arena_full())?,
        needed: arena
            .alloc_slice_copy(problem.needed)
            .map_err(|_| arena_full())?,
    }))
}

fn arena_full() -> SqlError {
    crate::sql::eval::arena_full()
}

fn arena_slice<T: Copy>(
    arena: &Arena,
    len: usize,
    fill: impl FnMut(usize) -> T,
) -> Result<&mut [T], SqlError> {
    arena.alloc_slice_with(len, fill).map_err(|_| arena_full())
}

/// What `add_paths_to_joinrel` reads of one join.
struct JoinInputs {
    outer: Relids,
    inner: Relids,
    restrict: ClauseList,
    /// `select_mergejoin_clauses`' mergeable clauses, in restriction order.
    mergeable: ClauseList,
    /// `innerrel_is_unique`, and then `compute_semi_anti_join_factors`:
    /// the fraction of outer rows with a match and the matches each has.
    inner_unique: bool,
    match_fraction: f64,
    match_count: f64,
}

impl<'p, 'a, 's> Search<'p, 'a, 's> {
    fn push_path(&mut self, path: Path) -> Result<u32, SqlError> {
        let id = self.paths.len() as u32;
        self.paths.push(path).map_err(|_| arena_full())?;
        Ok(id)
    }

    fn path(&self, id: u32) -> Path {
        self.paths.borrowed_slice()[id as usize]
    }

    fn clause(&self, id: u8) -> Clause {
        self.clauses.borrowed_slice()[usize::from(id)]
    }

    fn rel(&self, relids: Relids) -> Rel {
        self.rels[relids as usize].expect("the relation was built")
    }

    /// `old_rel->joininfo != NIL || old_rel->has_eclass_joins`: a class
    /// with a constant counts, though it derives no join clause.
    fn has_joins(&self, relids: Relids) -> bool {
        let clause_join = self
            .problem
            .clauses
            .iter()
            .any(|clause| clause.relids & relids != 0 && clause.relids & !relids != 0);
        let class_join = self
            .problem
            .classes
            .iter()
            .any(|class| class.relids() & relids != 0 && class.relids() & !relids != 0);
        clause_join || class_join
    }

    /// `have_relevant_joinclause`.
    fn relevant(&self, left: Relids, right: Relids) -> bool {
        let joined = self.problem.clauses.iter().any(|clause| {
            clause.relids & left != 0
                && clause.relids & right != 0
                && clause.relids & !(left | right) == 0
        });
        joined
            || self
                .problem
                .classes
                .iter()
                .any(|class| class.relids() & left != 0 && class.relids() & right != 0)
    }

    /// `join_search_one_level`.
    fn one_level(&mut self, level: usize) -> Result<Result<(), Outside>, SqlError> {
        let previous = self.levels[level - 1].as_slice();
        let singles = self.levels[1].as_slice();
        for (position, &old) in previous.iter().enumerate() {
            if self.has_joins(old) {
                let first = if level == 2 { position + 1 } else { 0 };
                for &other in &singles[first.min(singles.len())..] {
                    if old & other == 0 && self.relevant(old, other) {
                        self.make_join_rel(old, other, level)?;
                    }
                }
            } else {
                for &other in singles {
                    if old & other == 0 {
                        self.make_join_rel(old, other, level)?;
                    }
                }
            }
        }
        let mut k = 2;
        while k <= level - k {
            let other_level = level - k;
            let lower = self.levels[k].as_slice();
            let upper = self.levels[other_level].as_slice();
            for (position, &old) in lower.iter().enumerate() {
                if !self.has_joins(old) {
                    continue;
                }
                let first = if k == other_level { position + 1 } else { 0 };
                for &new in &upper[first.min(upper.len())..] {
                    if old & new == 0 && self.relevant(old, new) {
                        self.make_join_rel(old, new, level)?;
                    }
                }
            }
            k += 1;
        }
        if self.levels[level].is_empty() {
            for &old in previous {
                for &other in singles {
                    if old & other == 0 {
                        self.make_join_rel(old, other, level)?;
                    }
                }
            }
            if self.levels[level].is_empty() {
                return Ok(Err("a join search that builds no join"));
            }
        }
        Ok(Ok(()))
    }

    /// `make_join_rel` of an inner join: builds the relation on its first
    /// pair, sized by that pair's clauses, and adds both orders' paths.
    fn make_join_rel(&mut self, left: Relids, right: Relids, level: usize) -> Result<(), SqlError> {
        let relids = left | right;
        let restrict = self.restrictlist(left, right)?;
        if self.rels[relids as usize].is_none() {
            let (outer, inner) = (self.rel(left), self.rel(right));
            let rows = self.join_size(left, right, outer.rows, inner.rows, &restrict)?;
            self.rels[relids as usize] = Some(Rel {
                rows,
                width: self.width(relids),
                paths: [0; MAX_REL_PATHS],
                count: 0,
                cheapest_total: 0,
                cheapest_startup: 0,
                parameterized: [0; MAX_REL_PATHS],
                parameterized_count: 0,
                first: (left, right),
            });
            self.levels[level].push(relids).map_err(|_| arena_full())?;
        }
        for (outer, inner) in [(left, right), (right, left)] {
            let restrict = self.restrictlist(outer, inner)?;
            let mut mergeable = ClauseList::EMPTY;
            if self.problem.settings.mergejoin {
                for &id in restrict.as_slice() {
                    if matches!(self.clause(id).form, ClauseForm::Equality { .. }) {
                        mergeable.push(id);
                    }
                }
            }
            let inner_unique = self.inner_unique(inner, &restrict);
            let (match_fraction, match_count) = if inner_unique {
                self.semi_factors(inner, &restrict)?
            } else {
                (1.0, 1.0)
            };
            let inputs = JoinInputs {
                outer,
                inner,
                restrict,
                mergeable,
                inner_unique,
                match_fraction,
                match_count,
            };
            self.sort_inner_and_outer(relids, &inputs)?;
            self.match_unsorted_outer(relids, &inputs)?;
            if self.problem.settings.hashjoin {
                self.hash_inner_and_outer(relids, &inputs)?;
            }
        }
        Ok(())
    }

    /// `build_joinrel_restrictlist`: the clauses joining `outer` and
    /// `inner` (the outer relation's, then the inner's), then one equality
    /// per equivalence class spanning them
    /// (`generate_join_implied_equalities`).
    fn restrictlist(&mut self, outer: Relids, inner: Relids) -> Result<ClauseList, SqlError> {
        let joined = outer | inner;
        let mut list = ClauseList::EMPTY;
        for side in [outer, inner] {
            for (id, clause) in self.problem.clauses.iter().enumerate() {
                let spans = clause.relids & joined == clause.relids
                    && clause.relids & outer != 0
                    && clause.relids & inner != 0;
                if matches!(clause.form, ClauseForm::Other)
                    && spans
                    && clause.relids & side != 0
                    && !list.contains(id as u8)
                {
                    list.push(id as u8).ok_or_else(too_many)?;
                }
            }
        }
        for (class_id, class) in self.problem.classes.iter().enumerate() {
            if class.has_const {
                continue;
            }
            let members = class.members();
            let outer_member = members
                .iter()
                .position(|member| outer & 1 << member.rel != 0);
            let inner_member = members
                .iter()
                .position(|member| inner & 1 << member.rel != 0);
            let (Some(outer_member), Some(inner_member)) = (outer_member, inner_member) else {
                continue;
            };
            let id = self.class_clause(class_id as u8, outer_member as u8, inner_member as u8)?;
            list.push(id).ok_or_else(too_many)?;
        }
        Ok(list)
    }

    /// `create_join_clause`: the class's derived clause equating the two
    /// members in either orientation, else a new one oriented outer member
    /// first.
    fn class_clause(&mut self, class: u8, outer: u8, inner: u8) -> Result<u8, SqlError> {
        // The query's own equalities are not searched: PostgreSQL derives
        // a clause for each split, oriented as the split that first needed
        // it, and reuses it in either orientation.
        let found = self.clauses.borrowed_slice().iter().position(|clause| {
            clause.source.is_none()
                && matches!(clause.form, ClauseForm::Equality { class: c, left, right }
                    if c == class && ((left, right) == (outer, inner) || (left, right) == (inner, outer)))
        });
        if let Some(id) = found {
            return u8::try_from(id).map_err(|_| too_many());
        }
        let members = self.problem.classes[usize::from(class)].members();
        let relids = 1 << members[usize::from(outer)].rel | 1 << members[usize::from(inner)].rel;
        let cost = self
            .clauses
            .borrowed_slice()
            .iter()
            .find(
                |clause| matches!(clause.form, ClauseForm::Equality { class: c, .. } if c == class),
            )
            .map_or(self.problem.settings.costs.cpu_operator, |clause| {
                clause.cost
            });
        let id = self.clauses.len();
        if id >= MAX_CLAUSES {
            return Err(too_many());
        }
        self.clauses
            .push(Clause {
                form: ClauseForm::Equality {
                    class,
                    left: outer,
                    right: inner,
                },
                relids,
                cost,
                selectivity: None,
                source: None,
            })
            .map_err(|_| arena_full())?;
        Ok(id as u8)
    }

    /// `clause_selectivity` of a join clause for an inner join, cached on
    /// the clause as PostgreSQL caches it on its `RestrictInfo`.
    fn selectivity(&mut self, id: u8) -> Result<f64, SqlError> {
        let clause = self.clause(id);
        if let Some(selectivity) = clause.selectivity {
            return Ok(selectivity);
        }
        let selectivity = match clause.form {
            ClauseForm::Equality { class, left, right } => {
                let members = self.problem.classes[usize::from(class)].members();
                self.statistics
                    .equality(members[usize::from(left)], members[usize::from(right)])?
                    .clamp(0.0, 1.0)
            }
            ClauseForm::Other => 1.0,
        };
        let held = self.clauses.borrowed_slice()[usize::from(id)];
        self.clauses.truncate(self.clauses.len());
        self.set_clause(
            id,
            Clause {
                selectivity: Some(selectivity),
                ..held
            },
        )?;
        Ok(selectivity)
    }

    fn set_clause(&mut self, id: u8, clause: Clause) -> Result<(), SqlError> {
        // The list never published a slice, so its entries are writable.
        let mut rebuilt = ArenaList::new(self.arena);
        for (at, held) in self.clauses.borrowed_slice().iter().enumerate() {
            rebuilt
                .push(if at == usize::from(id) { clause } else { *held })
                .map_err(|_| arena_full())?;
        }
        self.clauses = rebuilt;
        Ok(())
    }

    /// The width of a join relation's target list: each needed column of
    /// its members that a relation outside it or the output reads.
    fn width(&self, relids: Relids) -> f64 {
        self.problem
            .needed
            .iter()
            .filter(|column| {
                relids & 1 << column.rel != 0 && (column.output || column.needed_by & !relids != 0)
            })
            .map(|column| column.width)
            .sum()
    }

    /// `set_cheapest`.
    fn set_cheapest(&mut self, relids: Relids) {
        let mut rel = self.rel(relids);
        let mut total: Option<u32> = None;
        let mut startup: Option<u32> = None;
        let mut best_param: Option<u32> = None;
        let mut parameterized = [0u32; MAX_REL_PATHS];
        let mut parameterized_count = 1usize;
        for &id in &rel.paths[..usize::from(rel.count)] {
            let path = self.path(id);
            if path.param != 0 {
                parameterized[parameterized_count] = id;
                parameterized_count += 1;
                if total.is_some() {
                    continue;
                }
                best_param = Some(match best_param {
                    None => id,
                    Some(best) => {
                        let held = self.path(best);
                        if path.param == held.param {
                            if compare_costs(&path, &held, false) < 0 {
                                id
                            } else {
                                best
                            }
                        } else if path.param & !held.param == 0 {
                            id
                        } else {
                            best
                        }
                    }
                });
                continue;
            }
            let Some(best_total) = total else {
                total = Some(id);
                startup = Some(id);
                continue;
            };
            let best_startup = startup.expect("set with the total");
            let compared = compare_costs(&self.path(best_startup), &path, true);
            if compared > 0
                || (compared == 0
                    && compare_keys(&self.path(best_startup).keys, &path.keys)
                        == KeysOrder::Better2)
            {
                startup = Some(id);
            }
            let compared = compare_costs(&self.path(best_total), &path, false);
            if compared > 0
                || (compared == 0
                    && compare_keys(&self.path(best_total).keys, &path.keys) == KeysOrder::Better2)
            {
                total = Some(id);
            }
        }
        let (total, startup) = match total {
            Some(total) => {
                parameterized[0] = total;
                (total, startup.expect("set with the total"))
            }
            None => {
                let best = best_param.expect("a relation has a path");
                parameterized.copy_within(1..parameterized_count, 0);
                parameterized_count -= 1;
                (best, best)
            }
        };
        rel.cheapest_total = total;
        rel.cheapest_startup = startup;
        rel.parameterized = parameterized;
        rel.parameterized_count = parameterized_count as u8;
        self.rels[relids as usize] = Some(rel);
    }

    /// `add_path` without parallel paths. A parameterized path's pathkeys
    /// do not count, and one path dominates another only when it needs no
    /// more of the other relations (`bms_subset_compare`).
    fn add_path(&mut self, relids: Relids, new: Path) -> Result<(), SqlError> {
        let mut rel = self.rel(relids);
        let mut kept = [0u32; MAX_REL_PATHS];
        let mut kept_count = 0usize;
        let mut accept = true;
        let mut insert_at = 0usize;
        let held = rel.paths;
        let count = usize::from(rel.count);
        let mut position = 0;
        while position < count {
            let old_id = held[position];
            let old = self.path(old_id);
            position += 1;
            let mut remove_old = false;
            let costs = compare_fuzzily(&new, &old, FUZZ);
            if costs != CostOrder::Different {
                let new_keys = if new.param != 0 { Keys::NONE } else { new.keys };
                let old_keys = if old.param != 0 { Keys::NONE } else { old.keys };
                let keys = compare_keys(&new_keys, &old_keys);
                // `bms_subset_compare` of the parameterizations.
                let outer = match (new.param & !old.param == 0, old.param & !new.param == 0) {
                    (true, true) => Subset::Equal,
                    (true, false) => Subset::Subset1,
                    (false, true) => Subset::Subset2,
                    (false, false) => Subset::Different,
                };
                let new_wins = matches!(outer, Subset::Equal | Subset::Subset1);
                let old_wins = matches!(outer, Subset::Equal | Subset::Subset2);
                if keys != KeysOrder::Different {
                    match costs {
                        CostOrder::Equal => match keys {
                            KeysOrder::Better1 => remove_old = new_wins && new.rows <= old.rows,
                            KeysOrder::Better2 => {
                                if old_wins && new.rows >= old.rows {
                                    accept = false;
                                }
                            }
                            KeysOrder::Equal => match outer {
                                Subset::Equal => {
                                    if new.rows < old.rows {
                                        remove_old = true;
                                    } else if new.rows > old.rows {
                                        accept = false;
                                    } else if compare_fuzzily(&new, &old, 1.000_000_000_1)
                                        == CostOrder::Better1
                                    {
                                        remove_old = true;
                                    } else {
                                        accept = false;
                                    }
                                }
                                Subset::Subset1 => remove_old = new.rows <= old.rows,
                                Subset::Subset2 => {
                                    if new.rows >= old.rows {
                                        accept = false;
                                    }
                                }
                                Subset::Different => {}
                            },
                            KeysOrder::Different => {}
                        },
                        CostOrder::Better1 => {
                            if keys != KeysOrder::Better2 && new_wins && new.rows <= old.rows {
                                remove_old = true;
                            }
                        }
                        CostOrder::Better2 => {
                            if keys != KeysOrder::Better1 && old_wins && new.rows >= old.rows {
                                accept = false;
                            }
                        }
                        CostOrder::Different => {}
                    }
                }
            }
            if !remove_old {
                if new.disabled > old.disabled
                    || (new.disabled == old.disabled && new.total >= old.total)
                {
                    insert_at = kept_count + 1;
                }
                kept[kept_count] = old_id;
                kept_count += 1;
            }
            if !accept {
                // The rest stay as they were.
                while position < count {
                    kept[kept_count] = held[position];
                    kept_count += 1;
                    position += 1;
                }
                break;
            }
        }
        if accept {
            if kept_count == MAX_REL_PATHS {
                return Err(too_many());
            }
            let id = self.push_path(new)?;
            kept.copy_within(insert_at..kept_count, insert_at + 1);
            kept[insert_at] = id;
            kept_count += 1;
        }
        rel.paths = kept;
        rel.count = kept_count as u8;
        self.rels[relids as usize] = Some(rel);
        Ok(())
    }

    /// `build_join_pathkeys` for an inner join: the outer keys still
    /// useful for merging above it (`truncate_useless_pathkeys`).
    fn join_keys(&self, joined: Relids, outer: &Keys) -> Keys {
        let mut useful = 0;
        for &class in outer.as_slice() {
            let class = &self.problem.classes[usize::from(class)];
            // `eclass_useful_for_merging`: a member outside the relation.
            if !class.has_const && class.count > 1 && class.relids() & !joined != 0 {
                useful += 1;
            } else {
                break;
            }
        }
        outer.truncated(useful)
    }

    fn outer_class(&self, id: u8) -> u8 {
        match self.clause(id).form {
            ClauseForm::Equality { class, .. } => class,
            ClauseForm::Other => unreachable!("only equalities merge"),
        }
    }

    /// `sort_inner_and_outer`.
    fn sort_inner_and_outer(
        &mut self,
        joined: Relids,
        inputs: &JoinInputs,
    ) -> Result<(), SqlError> {
        if inputs.mergeable.len == 0 {
            return Ok(());
        }
        let outer_path = self.rel(inputs.outer).cheapest_total;
        let inner_path = self.rel(inputs.inner).cheapest_total;
        // `select_outer_pathkeys_for_merge`, without query pathkeys.
        let mut classes = [0u8; MAX_JOIN_CLAUSES];
        let mut scores = [0i32; MAX_JOIN_CLAUSES];
        let mut count = 0;
        for &id in inputs.mergeable.as_slice() {
            let class = self.outer_class(id);
            if classes[..count].contains(&class) {
                continue;
            }
            scores[count] = self.problem.classes[usize::from(class)]
                .members()
                .iter()
                .filter(|member| joined & 1 << member.rel == 0)
                .count() as i32;
            classes[count] = class;
            count += 1;
        }
        let mut all = Keys::NONE;
        loop {
            let mut best = 0;
            for j in 1..count {
                if scores[j] > scores[best] {
                    best = j;
                }
            }
            if count == 0 || scores[best] < 0 {
                break;
            }
            scores[best] = -1;
            all.push(classes[best]).ok_or_else(too_many)?;
        }
        for front in 0..usize::from(all.len) {
            let mut outer_keys = Keys::NONE;
            outer_keys.push(all.classes[front]);
            for (at, &class) in all.as_slice().iter().enumerate() {
                if at != front {
                    outer_keys.push(class);
                }
            }
            let merge = self.mergeclauses_for_outer_keys(&outer_keys, &inputs.mergeable);
            // `make_inner_pathkeys_for_merge`: the same classes.
            let used = self.used_keys(&outer_keys, &merge);
            let keys = self.join_keys(joined, &outer_keys);
            self.try_mergejoin(
                joined,
                inputs,
                outer_path,
                inner_path,
                keys,
                merge,
                Some(outer_keys),
                Some(used),
            )?;
        }
        Ok(())
    }

    /// `find_mergeclauses_for_outer_pathkeys`.
    fn mergeclauses_for_outer_keys(&self, keys: &Keys, mergeable: &ClauseList) -> ClauseList {
        let mut merge = ClauseList::EMPTY;
        for &class in keys.as_slice() {
            let mut matched = false;
            for &id in mergeable.as_slice() {
                if self.outer_class(id) == class {
                    merge.push(id);
                    matched = true;
                }
            }
            if !matched {
                break;
            }
        }
        merge
    }

    /// The leading keys of `keys` the merge clauses use.
    fn used_keys(&self, keys: &Keys, merge: &ClauseList) -> Keys {
        let mut used = Keys::NONE;
        for &id in merge.as_slice() {
            let class = self.outer_class(id);
            if used.as_slice().last() != Some(&class) {
                used.push(class);
            }
        }
        debug_assert!(used.contained_in(keys));
        used
    }

    /// `match_unsorted_outer` for an inner join.
    fn match_unsorted_outer(
        &mut self,
        joined: Relids,
        inputs: &JoinInputs,
    ) -> Result<(), SqlError> {
        let outer_rel = self.rel(inputs.outer);
        let inner_rel = self.rel(inputs.inner);
        let inner_total = inner_rel.cheapest_total;
        let settings = self.problem.settings;
        let material = if settings.material && !self.materializes_output(inner_total) {
            Some(self.material_path(inner_total)?)
        } else {
            None
        };
        for &outer in &outer_rel.paths[..usize::from(outer_rel.count)] {
            // An outer path reading the inner relation's values cannot be.
            if self.path(outer).param & inputs.inner != 0 {
                continue;
            }
            let keys = self.join_keys(joined, &self.path(outer).keys);
            // The cheapest inner path of each parameterization, the
            // unparameterized one first.
            for &inner in &inner_rel.parameterized[..usize::from(inner_rel.parameterized_count)] {
                self.try_nestloop(joined, inputs, outer, inner, keys)?;
                if let Some(memoize) = self.memoize_path(inputs, outer, inner)? {
                    self.try_nestloop(joined, inputs, outer, memoize, keys)?;
                }
            }
            if let Some(material) = material {
                self.try_nestloop(joined, inputs, outer, material, keys)?;
            }
            // A parameterized outer path's merge joins stay parameterized,
            // which an inner join's search rejects.
            if self.path(outer).param == 0 {
                self.generate_mergejoin_paths(joined, inputs, outer, inner_total, keys)?;
            }
        }
        Ok(())
    }

    /// `calc_joinrel_size_estimate` of an inner join of `left` and `right`
    /// rows by `restrict`, with `get_foreign_key_join_selectivity`: a
    /// foreign key whose every class the clauses equate makes each
    /// referencing row match one referenced row, and its clauses leave the
    /// estimate.
    fn join_size(
        &mut self,
        left: Relids,
        right: Relids,
        left_rows: f64,
        right_rows: f64,
        restrict: &ClauseList,
    ) -> Result<f64, SqlError> {
        let mut kept = *restrict;
        let mut foreign = 1.0;
        for key in self.problem.foreign_keys {
            let spans = (left & 1 << key.referencing != 0 && right & 1 << key.referenced != 0)
                || (left & 1 << key.referenced != 0 && right & 1 << key.referencing != 0);
            if !spans {
                continue;
            }
            let classes = &key.classes[..usize::from(key.count)];
            let mut remaining = ClauseList::EMPTY;
            let mut removed = 0;
            for &id in kept.as_slice() {
                match self.clause(id).form {
                    ClauseForm::Equality { class, .. } if classes.contains(&class) => removed += 1,
                    _ => {
                        remaining.push(id);
                    }
                }
            }
            if removed == 0 || removed != classes.len() {
                continue;
            }
            kept = remaining;
            foreign *= 1.0 / key.referenced_tuples;
        }
        let foreign = foreign.clamp(0.0, 1.0);
        let mut selectivity = 1.0;
        for &id in kept.as_slice() {
            selectivity *= self.selectivity(id)?;
        }
        Ok(clamp_row_est(
            left_rows * right_rows * foreign * selectivity,
        ))
    }

    /// `innerrel_is_unique` of an inner join: the inner relation is one
    /// base relation with a unique index whose every key column a join
    /// clause or a constant equality fixes (`relation_has_unique_index_ext`).
    fn inner_unique(&self, inner: Relids, restrict: &ClauseList) -> bool {
        if restrict.len == 0 || inner.count_ones() != 1 {
            return false;
        }
        let base = &self.problem.rels[inner.trailing_zeros() as usize];
        if base.unique.is_empty() {
            return false;
        }
        let mut matched = base.fixed;
        for &id in restrict.as_slice() {
            if let ClauseForm::Equality { class, left, right } = self.clause(id).form {
                let members = self.problem.classes[usize::from(class)].members();
                for member in [members[usize::from(left)], members[usize::from(right)]] {
                    if inner & 1 << member.rel != 0 && member.column < 64 {
                        matched |= 1 << member.column;
                    }
                }
            }
        }
        base.unique.iter().any(|&keys| keys & !matched == 0)
    }

    /// `compute_semi_anti_join_factors` of an inner join whose inner
    /// relation is unique. Its `sjinfo` is the inner join's, so `eqjoinsel`
    /// answers the semijoin selectivity with the inner one: every outer row
    /// matches by that selectivity, each with the inner relation's rows.
    fn semi_factors(
        &mut self,
        inner: Relids,
        restrict: &ClauseList,
    ) -> Result<(f64, f64), SqlError> {
        let mut selectivity = 1.0;
        for &id in restrict.as_slice() {
            selectivity *= self.selectivity(id)?;
        }
        let count = if selectivity > 0.0 {
            (selectivity * self.rel(inner).rows / selectivity).max(1.0)
        } else {
            1.0
        };
        Ok((selectivity, count))
    }

    /// `get_memoize_path`: a cache over a parameterized inner path whose
    /// every moved join clause hashes, read once per outer row.
    fn memoize_path(
        &mut self,
        inputs: &JoinInputs,
        outer: u32,
        inner: u32,
    ) -> Result<Option<u32>, SqlError> {
        let settings = self.problem.settings;
        let inner_path = self.path(inner);
        // A parameterized join's `ppi_clauses` are empty: it gets no cache.
        if !matches!(inner_path.kind, PathKind::Scan { .. })
            || !settings.memoize
            || self.rel(inputs.outer).rows < 2.0
            || inner_path.param == 0
            || inner_path.param & !inputs.outer != 0
        {
            return Ok(None);
        }
        let enforced = self.enforced(inner);
        if enforced.len == 0 || (inputs.inner_unique && enforced.len < inputs.restrict.len) {
            return Ok(None);
        }
        // The cache key: each moved clause's outer side, once.
        let mut keys = [Member { rel: 0, column: 0 }; MAX_KEYS];
        let mut key_count = 0usize;
        let mut width = 0.0;
        for &id in enforced.as_slice() {
            let ClauseForm::Equality { class, left, right } = self.clause(id).form else {
                return Ok(None);
            };
            let members = self.problem.classes[usize::from(class)].members();
            let Some(key) = [members[usize::from(left)], members[usize::from(right)]]
                .into_iter()
                .find(|member| inputs.outer & 1 << member.rel != 0)
            else {
                return Ok(None);
            };
            if !keys[..key_count].contains(&key) {
                if key_count == MAX_KEYS {
                    return Ok(None);
                }
                keys[key_count] = key;
                key_count += 1;
                width += self.column_width(key);
            }
        }
        let costs = settings.costs;
        let outer_path = self.path(outer);
        let calls = clamp_row_est(outer_path.rows);
        // `cost_memoize_rescan`.
        let tuples = inner_path.rows;
        let entry_bytes =
            relation_byte_size(tuples, inner_path.width) + 48.0 + 16.0 * tuples + width;
        let cache_entries = (settings.hash_mem_bytes / entry_bytes).floor();
        let distinct = self.statistics.groups(&keys[..key_count], calls)?;
        let evict_ratio = 1.0 - cache_entries.min(distinct) / distinct;
        let hit_ratio =
            ((calls - distinct) / calls) * (cache_entries / distinct.max(cache_entries));
        let mut rescan_total = inner_path.total * (1.0 - hit_ratio) + costs.cpu_operator;
        rescan_total += costs.cpu_tuple * evict_ratio;
        rescan_total += costs.cpu_operator / 10.0 * evict_ratio * tuples;
        rescan_total += costs.cpu_tuple + costs.cpu_operator * tuples;
        let rescan_startup = inner_path.startup * (1.0 - hit_ratio) + costs.cpu_tuple;
        let id = self.push_path(Path {
            kind: PathKind::Memoize {
                input: inner,
                keys,
                key_count: key_count as u8,
                single_row: inputs.inner_unique,
            },
            startup: inner_path.startup + costs.cpu_tuple,
            total: inner_path.total + costs.cpu_tuple,
            ..inner_path
        })?;
        self.memoize_rescans
            .push((id, rescan_startup, rescan_total))
            .map_err(|_| arena_full())?;
        Ok(Some(id))
    }

    /// The width of a base relation's column, as its target list has it.
    fn column_width(&self, member: Member) -> f64 {
        self.problem
            .needed
            .iter()
            .find(|column| column.rel == member.rel && column.column == member.column)
            .map_or(0.0, |column| column.width)
    }

    /// The join clauses a parameterized path enforces
    /// (`get_param_path_clause_serials`): a base path's moved clauses, a
    /// parameterized join's own clauses and its parameterized inputs'.
    fn enforced(&self, id: u32) -> ClauseList {
        let path = self.path(id);
        if path.param == 0 {
            return ClauseList::EMPTY;
        }
        match path.kind {
            PathKind::Scan { rel, path } => {
                self.problem.rels[usize::from(rel)].paths[usize::from(path)].enforced
            }
            PathKind::Material { input } | PathKind::Memoize { input, .. } => self.enforced(input),
            PathKind::NestLoop {
                outer,
                inner,
                clauses,
                ..
            } => {
                let mut all = self.enforced(outer);
                for list in [self.enforced(inner), clauses] {
                    for &id in list.as_slice() {
                        if !all.contains(id) {
                            let _ = all.push(id);
                        }
                    }
                }
                all
            }
            PathKind::HashJoin { .. } | PathKind::MergeJoin { .. } => ClauseList::EMPTY,
        }
    }

    /// `get_joinrel_parampathinfo`'s clauses of a join parameterized by
    /// `required` whose inputs read `outer_param` and `inner_param`: per
    /// class, its first member among `required` equated with its first in
    /// the join, unless an input can apply that clause; then, for each class
    /// whose clause the inner path applies, its first member among
    /// `required` equated with its first in the outer relation, unless the
    /// outer path applies it.
    fn moved_join_clauses(
        &mut self,
        joined: Relids,
        inputs: &JoinInputs,
        (outer_param, inner_param): (Relids, Relids),
        required: Relids,
    ) -> Result<ClauseList, SqlError> {
        // `join_clause_is_movable_into` an input reading `param`.
        let movable = |input: Relids, param: Relids, one: u8, two: u8| {
            let relids = 1 << one | 1 << two;
            param != 0 && relids & !(input | param) == 0 && relids & input != 0
        };
        let mut moved = ClauseList::EMPTY;
        let mut dropped = [0u8; MAX_JOIN_CLAUSES];
        let mut dropped_count = 0;
        let first = |members: &[Member], relids: Relids| {
            members
                .iter()
                .position(|member| relids & 1 << member.rel != 0)
        };
        for class_id in 0..self.problem.classes.len() {
            let class = self.problem.classes[class_id];
            if class.has_const {
                continue;
            }
            let members = class.members();
            let (Some(outside), Some(within)) = (first(members, required), first(members, joined))
            else {
                continue;
            };
            let (one, two) = (members[outside].rel, members[within].rel);
            if movable(inputs.outer, outer_param, one, two) {
                continue;
            }
            if movable(inputs.inner, inner_param, one, two) {
                if dropped_count < dropped.len() {
                    dropped[dropped_count] = class_id as u8;
                    dropped_count += 1;
                }
                continue;
            }
            let id = self.class_clause(class_id as u8, outside as u8, within as u8)?;
            moved.push(id).ok_or_else(too_many)?;
        }
        for &class_id in &dropped[..dropped_count] {
            let members = self.problem.classes[usize::from(class_id)].members();
            let (Some(outside), Some(outer)) =
                (first(members, required), first(members, inputs.outer))
            else {
                continue;
            };
            if movable(
                inputs.outer,
                outer_param,
                members[outside].rel,
                members[outer].rel,
            ) {
                continue;
            }
            let id = self.class_clause(class_id, outside as u8, outer as u8)?;
            moved.push(id).ok_or_else(too_many)?;
        }
        Ok(moved)
    }

    /// `has_indexed_join_quals` of a nested loop over `inner` keeping the
    /// join clauses `clauses`.
    fn indexed_join_quals(&self, inner: u32, clauses: &ClauseList) -> bool {
        if clauses.len != 0 {
            return false;
        }
        match self.path(inner).kind {
            PathKind::Scan { rel, path } => {
                self.problem.rels[usize::from(rel)].paths[usize::from(path)].indexed_join_quals
            }
            _ => false,
        }
    }

    /// `ExecMaterializesOutput`.
    fn materializes_output(&self, id: u32) -> bool {
        matches!(self.path(id).kind, PathKind::Material { .. })
    }

    /// `create_material_path` with `cost_material`.
    fn material_path(&mut self, input: u32) -> Result<u32, SqlError> {
        let path = self.path(input);
        let settings = self.problem.settings;
        let (startup, total) =
            material_cost(path.startup, path.total, path.rows, path.width, settings);
        self.push_path(Path {
            kind: PathKind::Material { input },
            startup,
            total,
            disabled: path.disabled + u32::from(!settings.material),
            ..path
        })
    }

    /// `generate_mergejoin_paths` for an inner join.
    fn generate_mergejoin_paths(
        &mut self,
        joined: Relids,
        inputs: &JoinInputs,
        outer: u32,
        inner_total: u32,
        keys: Keys,
    ) -> Result<(), SqlError> {
        let outer_keys = self.path(outer).keys;
        let merge = self.mergeclauses_for_outer_keys(&outer_keys, &inputs.mergeable);
        if merge.len == 0 {
            return Ok(());
        }
        let inner_keys = self.used_keys(&outer_keys, &merge);
        self.try_mergejoin(
            joined,
            inputs,
            outer,
            inner_total,
            keys,
            merge,
            None,
            Some(inner_keys),
        )?;
        let (mut cheapest_total, mut cheapest_startup) =
            if inner_keys.contained_in(&self.path(inner_total).keys) {
                (Some(inner_total), Some(inner_total))
            } else {
                (None, None)
            };
        let inner_rel = self.rel(inputs.inner);
        for count in (1..=usize::from(inner_keys.len)).rev() {
            let trial = inner_keys.truncated(count);
            let mut new_clauses = None;
            let total = self.cheapest_for_keys(&inner_rel, &trial, false);
            if let Some(path) = total
                && cheapest_total
                    .is_none_or(|held| compare_costs(&self.path(path), &self.path(held), false) < 0)
            {
                let clauses =
                    self.trim_mergeclauses(&merge, &trial, count < usize::from(inner_keys.len));
                new_clauses = Some(clauses);
                self.try_mergejoin(joined, inputs, outer, path, keys, clauses, None, None)?;
                cheapest_total = Some(path);
            }
            let startup = self.cheapest_for_keys(&inner_rel, &trial, true);
            if let Some(path) = startup
                && cheapest_startup
                    .is_none_or(|held| compare_costs(&self.path(path), &self.path(held), true) < 0)
            {
                if Some(path) != cheapest_total {
                    let clauses = new_clauses.unwrap_or_else(|| {
                        self.trim_mergeclauses(&merge, &trial, count < usize::from(inner_keys.len))
                    });
                    self.try_mergejoin(joined, inputs, outer, path, keys, clauses, None, None)?;
                }
                cheapest_startup = Some(path);
            }
        }
        Ok(())
    }

    /// `trim_mergeclauses_for_inner_pathkeys`.
    fn trim_mergeclauses(&self, merge: &ClauseList, keys: &Keys, trim: bool) -> ClauseList {
        if !trim {
            return *merge;
        }
        let mut trimmed = ClauseList::EMPTY;
        for &id in merge.as_slice() {
            if keys.as_slice().contains(&self.outer_class(id)) {
                trimmed.push(id);
            } else {
                break;
            }
        }
        trimmed
    }

    /// `get_cheapest_path_for_pathkeys`.
    fn cheapest_for_keys(&self, rel: &Rel, keys: &Keys, startup: bool) -> Option<u32> {
        let mut best: Option<u32> = None;
        for &id in &rel.paths[..usize::from(rel.count)] {
            let path = self.path(id);
            if path.param != 0 {
                continue;
            }
            if best.is_some_and(|best| compare_costs(&self.path(best), &path, startup) <= 0) {
                continue;
            }
            if keys.contained_in(&path.keys) {
                best = Some(id);
            }
        }
        best
    }

    /// `try_nestloop_path` with `initial_cost_nestloop` and
    /// `final_cost_nestloop` of an inner join without a unique inner.
    fn try_nestloop(
        &mut self,
        joined: Relids,
        inputs: &JoinInputs,
        outer: u32,
        inner: u32,
        keys: Keys,
    ) -> Result<(), SqlError> {
        let settings = self.problem.settings;
        let (outer_path, inner_path) = (self.path(outer), self.path(inner));
        // `calc_nestloop_required_outer`: what the join still reads from
        // other relations, which an inner join's search rejects unless
        // `allow_star_schema_join` lets it build a parameterized join.
        let required = (outer_path.param | inner_path.param) & !inputs.outer;
        let mut moved = ClauseList::EMPTY;
        if required != 0 {
            let star =
                inner_path.param & inputs.outer != 0 && inner_path.param & !inputs.outer != 0;
            if !star {
                return Ok(());
            }
            moved = self.moved_join_clauses(
                joined,
                inputs,
                (outer_path.param, inner_path.param),
                required,
            )?;
        }
        // The join clauses the inner path enforces leave the join's; a
        // parameterized join's moved clauses come first
        // (`get_joinrel_parampathinfo`).
        let mut clauses = moved;
        let enforced = if inner_path.param & inputs.outer != 0 {
            self.enforced(inner)
        } else {
            ClauseList::EMPTY
        };
        for &id in inputs.restrict.as_slice() {
            if !enforced.contains(id) {
                clauses.push(id).ok_or_else(too_many)?;
            }
        }
        // `ppi_rows`: the first estimate for the join's parameterization.
        let param_rows = if required == 0 {
            None
        } else if let Some(&(_, _, rows)) = self
            .param_rows
            .borrowed_slice()
            .iter()
            .find(|(relids, param, _)| *relids == joined && *param == required)
        {
            Some(rows)
        } else {
            let rows = self
                .join_size(
                    inputs.outer,
                    inputs.inner,
                    outer_path.rows,
                    inner_path.rows,
                    &clauses,
                )?
                .min(self.rel(joined).rows);
            self.param_rows
                .push((joined, required, rows))
                .map_err(|_| arena_full())?;
            Some(rows)
        };
        let (rescan_startup, rescan_total) = self.rescan_cost(inner);
        let mut startup = outer_path.startup + inner_path.startup;
        let mut run = outer_path.total - outer_path.startup;
        if outer_path.rows > 1.0 {
            run += (outer_path.rows - 1.0) * rescan_startup;
        }
        let inner_run = inner_path.total - inner_path.startup;
        let inner_rescan_run = rescan_total - rescan_startup;
        if !inputs.inner_unique {
            run += inner_run;
            if outer_path.rows > 1.0 {
                run += (outer_path.rows - 1.0) * inner_rescan_run;
            }
        }
        let outer_rows = if outer_path.rows <= 0.0 {
            1.0
        } else {
            outer_path.rows
        };
        let inner_rows = if inner_path.rows <= 0.0 {
            1.0
        } else {
            inner_path.rows
        };
        let rel = self.rel(joined);
        // `final_cost_nestloop`: a unique inner relation's scan stops at
        // the first match.
        let tuples = if inputs.inner_unique {
            let mut matched = (outer_rows * inputs.match_fraction).round_ties_even();
            let mut unmatched = outer_rows - matched;
            let scan_fraction = 2.0 / (inputs.match_count + 1.0);
            let mut tuples = matched * inner_rows * scan_fraction;
            if self.indexed_join_quals(inner, &clauses) {
                run += inner_run * scan_fraction;
                if matched > 1.0 {
                    run += (matched - 1.0) * inner_rescan_run * scan_fraction;
                }
                run += unmatched * inner_rescan_run / inner_rows;
            } else {
                tuples += unmatched * inner_rows;
                run += inner_run;
                if unmatched >= 1.0 {
                    unmatched -= 1.0;
                } else {
                    matched -= 1.0;
                }
                if matched > 0.0 {
                    run += matched * inner_rescan_run * scan_fraction;
                }
                if unmatched > 0.0 {
                    run += unmatched * inner_rescan_run;
                }
            }
            tuples
        } else {
            outer_rows * inner_rows
        };
        let qual = self.qual_cost(&clauses);
        startup += 0.0;
        run += (settings.costs.cpu_tuple + qual) * tuples;
        self.add_path(
            joined,
            Path {
                kind: PathKind::NestLoop {
                    outer,
                    inner,
                    clauses,
                    inner_unique: inputs.inner_unique,
                },
                relids: joined,
                rows: param_rows.unwrap_or(rel.rows),
                width: rel.width,
                startup,
                total: startup + run,
                disabled: u32::from(!settings.nestloop) + outer_path.disabled + inner_path.disabled,
                keys,
                param: required,
            },
        )
    }

    /// `cost_rescan`.
    fn rescan_cost(&self, id: u32) -> (f64, f64) {
        let path = self.path(id);
        let settings = self.problem.settings;
        match path.kind {
            PathKind::Material { .. } => {
                let mut run = settings.costs.cpu_operator * path.rows;
                let bytes = relation_byte_size(path.rows, path.width);
                if bytes > settings.work_mem_bytes {
                    run += settings.costs.seq_page * (bytes / BLOCK).ceil();
                }
                (0.0, run)
            }
            PathKind::HashJoin { batches: 1, .. } => (0.0, path.total - path.startup),
            PathKind::Memoize { .. } => self
                .memoize_rescans
                .borrowed_slice()
                .iter()
                .find(|(held, _, _)| *held == id)
                .map_or((path.startup, path.total), |&(_, startup, total)| {
                    (startup, total)
                }),
            _ => (path.startup, path.total),
        }
    }

    /// The per-tuple cost of evaluating `clauses`.
    fn qual_cost(&self, clauses: &ClauseList) -> f64 {
        clauses
            .as_slice()
            .iter()
            .map(|&id| self.clause(id).cost)
            .sum()
    }

    /// `try_mergejoin_path` with `initial_cost_mergejoin` and
    /// `final_cost_mergejoin` of an inner join. `outer_sort` and
    /// `inner_sort` are the keys to sort by, or `None` for an input
    /// already in order.
    #[allow(clippy::too_many_arguments)]
    fn try_mergejoin(
        &mut self,
        joined: Relids,
        inputs: &JoinInputs,
        outer: u32,
        inner: u32,
        keys: Keys,
        merge: ClauseList,
        outer_sort: Option<Keys>,
        inner_sort: Option<Keys>,
    ) -> Result<(), SqlError> {
        let settings = self.problem.settings;
        let (outer_path, inner_path) = (self.path(outer), self.path(inner));
        // A sort the input's order already satisfies is dropped.
        let outer_sort = outer_sort.filter(|sort| !sort.contained_in(&outer_path.keys));
        let inner_sort = inner_sort.filter(|sort| !sort.contained_in(&inner_path.keys));
        if outer_sort.is_some_and(|sort| sort.common_prefix(&outer_path.keys) > 0) {
            // An incremental sort of a presorted outer input.
            self.outside = Some("a merge join over a partly sorted outer input");
            return Ok(());
        }
        let outer_rows = outer_path.rows.max(1.0);
        let inner_rows_all = inner_path.rows.max(1.0);
        // `mergejoinscansel` of the first clause.
        let first = merge.as_slice()[0];
        let range = match self.clause(first).form {
            ClauseForm::Equality { class, left, right } => {
                let members = self.problem.classes[usize::from(class)].members();
                let (left, right) = (members[usize::from(left)], members[usize::from(right)]);
                let Some(range) = self.statistics.merge_range(left, right)? else {
                    self.outside = Some("a merge join range estimate outside the model");
                    return Ok(());
                };
                if inputs.outer & 1 << left.rel != 0 {
                    range
                } else {
                    [range[2], range[3], range[0], range[1]]
                }
            }
            ClauseForm::Other => [0.0, 1.0, 0.0, 1.0],
        };
        let [outer_start, outer_end, inner_start, inner_end] = range;
        let outer_skip = (outer_rows * outer_start).round_ties_even();
        let inner_skip = (inner_rows_all * inner_start).round_ties_even();
        let outer_rows_read = clamp_row_est(outer_rows * outer_end);
        let inner_rows_read = clamp_row_est(inner_rows_all * inner_end);
        let outer_start = outer_skip / outer_rows;
        let inner_start = inner_skip / inner_rows_all;
        let outer_end = outer_rows_read / outer_rows;
        let inner_end = inner_rows_read / inner_rows_all;
        let mut disabled = u32::from(!settings.mergejoin);
        let mut startup = 0.0;
        let mut run = 0.0;
        let (sorted_startup, sorted_total, sorted_disabled) = match outer_sort {
            Some(_) => self.sort_cost(&outer_path),
            None => (outer_path.startup, outer_path.total, outer_path.disabled),
        };
        disabled += sorted_disabled;
        startup += sorted_startup + (sorted_total - sorted_startup) * outer_start;
        run += (sorted_total - sorted_startup) * (outer_end - outer_start);
        let (sorted_startup, sorted_total, sorted_disabled) = match inner_sort {
            Some(_) => self.sort_cost(&inner_path),
            None => (inner_path.startup, inner_path.total, inner_path.disabled),
        };
        disabled += sorted_disabled;
        startup += sorted_startup + (sorted_total - sorted_startup) * inner_start;
        let inner_run = (sorted_total - sorted_startup) * (inner_end - inner_start);
        // `final_cost_mergejoin`.
        let merge_cost = self.qual_cost(&merge);
        let other_cost = self.qual_cost(&inputs.restrict) - merge_cost;
        let mut selectivity = 1.0;
        for &id in merge.as_slice() {
            selectivity *= self.selectivity(id)?;
        }
        let merged = clamp_row_est(selectivity * outer_path.rows * inner_path.rows);
        // A unique inner relation every clause merges needs no mark.
        let skip_mark_restore = inputs.inner_unique && inputs.restrict.len == merge.len;
        let rescanned = if skip_mark_restore {
            0.0
        } else {
            (merged - inner_path.rows).max(0.0)
        };
        let rescan_ratio = 1.0 + rescanned / inner_rows_read;
        let bare = inner_run * rescan_ratio;
        let materialized = inner_run + settings.costs.cpu_operator * inner_rows_read * rescan_ratio;
        let materialize_inner = if skip_mark_restore {
            false
        } else if (settings.material && materialized < bare)
            || (inner_sort.is_none() && !self.supports_mark_restore(inner))
        {
            true
        } else {
            settings.material
                && inner_sort.is_some()
                && relation_byte_size(inner_path.rows.max(1.0), inner_path.width)
                    > settings.work_mem_bytes
        };
        run += if materialize_inner {
            materialized
        } else {
            bare
        };
        startup += merge_cost * (outer_skip + inner_skip * rescan_ratio);
        run += merge_cost
            * ((outer_rows_read - outer_skip) + (inner_rows_read - inner_skip) * rescan_ratio);
        run += (settings.costs.cpu_tuple + other_cost) * merged;
        let rel = self.rel(joined);
        self.add_path(
            joined,
            Path {
                kind: PathKind::MergeJoin {
                    outer,
                    inner,
                    clauses: inputs.restrict,
                    merge,
                    outer_sort: outer_sort.unwrap_or(Keys::NONE),
                    inner_sort: inner_sort.unwrap_or(Keys::NONE),
                    materialize_inner,
                    inner_unique: inputs.inner_unique,
                },
                relids: joined,
                rows: rel.rows,
                width: rel.width,
                startup,
                total: startup + run,
                disabled,
                keys,
                param: 0,
            },
        )
    }

    /// `ExecSupportsMarkRestore` of a path's plan.
    fn supports_mark_restore(&self, id: u32) -> bool {
        match self.path(id).kind {
            PathKind::Material { .. } => true,
            PathKind::Scan { rel, path } => {
                self.problem.rels[usize::from(rel)].paths[usize::from(path)].mark_restore
            }
            _ => false,
        }
    }

    /// `cost_sort` of a path's rows: its startup and total cost.
    fn sort_cost(&self, path: &Path) -> (f64, f64, u32) {
        let (startup, run) =
            super::paths::tuplesort_cost(path.rows, path.width, None, self.problem.settings.costs);
        let startup = startup + path.total;
        (
            startup,
            startup + run,
            path.disabled + u32::from(!self.problem.settings.sort),
        )
    }

    /// `hash_inner_and_outer` for an inner join.
    fn hash_inner_and_outer(
        &mut self,
        joined: Relids,
        inputs: &JoinInputs,
    ) -> Result<(), SqlError> {
        let mut hash = ClauseList::EMPTY;
        for &id in inputs.restrict.as_slice() {
            if matches!(self.clause(id).form, ClauseForm::Equality { .. }) {
                hash.push(id);
            }
        }
        if hash.len == 0 {
            return Ok(());
        }
        let outer_rel = self.rel(inputs.outer);
        let inner_total = self.rel(inputs.inner).cheapest_total;
        self.try_hashjoin(
            joined,
            inputs,
            outer_rel.cheapest_startup,
            inner_total,
            hash,
        )?;
        if outer_rel.cheapest_total != outer_rel.cheapest_startup {
            self.try_hashjoin(joined, inputs, outer_rel.cheapest_total, inner_total, hash)?;
        }
        Ok(())
    }

    /// `try_hashjoin_path` with `initial_cost_hashjoin` and
    /// `final_cost_hashjoin` of an inner join without a unique inner.
    fn try_hashjoin(
        &mut self,
        joined: Relids,
        inputs: &JoinInputs,
        outer: u32,
        inner: u32,
        hash: ClauseList,
    ) -> Result<(), SqlError> {
        let settings = self.problem.settings;
        let costs = settings.costs;
        let (outer_path, inner_path) = (self.path(outer), self.path(inner));
        let clauses = f64::from(hash.len);
        let mut startup = outer_path.startup;
        let mut run = outer_path.total - outer_path.startup;
        startup += inner_path.total;
        startup += (costs.cpu_operator * clauses + costs.cpu_tuple) * inner_path.rows;
        run += costs.cpu_operator * clauses * outer_path.rows;
        let sizing = choose_hash_table_size(inner_path.rows, inner_path.width, settings);
        if sizing.batches > 1 {
            let outer_pages = page_size(outer_path.rows, outer_path.width);
            let inner_pages = page_size(inner_path.rows, inner_path.width);
            startup += costs.seq_page * inner_pages;
            run += costs.seq_page * (inner_pages + 2.0 * outer_pages);
        }
        // `final_cost_hashjoin`.
        let virtual_buckets = f64::from(sizing.buckets) * f64::from(sizing.batches);
        let mut bucket_size: f64 = 1.0;
        let mut mcv_frequency: f64 = 1.0;
        for &id in hash.as_slice() {
            let ClauseForm::Equality { class, left, right } = self.clause(id).form else {
                continue;
            };
            let members = self.problem.classes[usize::from(class)].members();
            let inner_member = [members[usize::from(left)], members[usize::from(right)]]
                .into_iter()
                .find(|member| inputs.inner & 1 << member.rel != 0)
                .expect("a hash clause reads the inner relation");
            let (frequency, size) = self.statistics.bucket(inner_member, virtual_buckets)?;
            bucket_size = bucket_size.min(size);
            mcv_frequency = mcv_frequency.min(frequency);
        }
        let mut disabled =
            u32::from(!settings.hashjoin) + outer_path.disabled + inner_path.disabled;
        if relation_byte_size(
            clamp_row_est(inner_path.rows * mcv_frequency),
            inner_path.width,
        ) > settings.hash_mem_bytes
        {
            // `disable_cost`, which PostgreSQL 18 adds to the startup cost.
            startup += 1.0e10;
            disabled += 0;
        }
        let hash_cost = self.qual_cost(&hash);
        let other_cost = self.qual_cost(&inputs.restrict) - hash_cost;
        let tuples = if inputs.inner_unique {
            // Each outer row stops at its first match.
            let matched = (outer_path.rows * inputs.match_fraction).round_ties_even();
            let scan_fraction = 2.0 / (inputs.match_count + 1.0);
            run += hash_cost
                * matched
                * clamp_row_est(inner_path.rows * bucket_size * scan_fraction)
                * 0.5;
            run += hash_cost
                * (outer_path.rows - matched)
                * clamp_row_est(inner_path.rows / virtual_buckets)
                * 0.05;
            matched
        } else {
            run += hash_cost * outer_path.rows * clamp_row_est(inner_path.rows * bucket_size) * 0.5;
            let mut selectivity = 1.0;
            for &id in hash.as_slice() {
                selectivity *= self.selectivity(id)?;
            }
            clamp_row_est(selectivity * outer_path.rows * inner_path.rows)
        };
        run += (costs.cpu_tuple + other_cost) * tuples;
        let rel = self.rel(joined);
        self.add_path(
            joined,
            Path {
                kind: PathKind::HashJoin {
                    outer,
                    inner,
                    clauses: inputs.restrict,
                    hash,
                    buckets: sizing.buckets,
                    batches: sizing.batches,
                },
                relids: joined,
                rows: rel.rows,
                width: rel.width,
                startup,
                total: startup + run,
                disabled,
                keys: Keys::NONE,
                param: 0,
            },
        )
    }
}

fn too_many() -> SqlError {
    crate::sql_err!(
        crate::sql::eval::sqlstate::FEATURE_NOT_SUPPORTED,
        "a join with more clauses or paths than the join search holds is not supported"
    )
}

/// `relation_byte_size`.
pub(crate) fn relation_byte_size(tuples: f64, width: f64) -> f64 {
    tuples * ((width as u64).next_multiple_of(8) as f64 + 24.0)
}

/// `cost_material` of an input of `startup` and `total` cost.
pub(crate) fn material_cost(
    startup: f64,
    total: f64,
    rows: f64,
    width: f64,
    settings: JoinSettings,
) -> (f64, f64) {
    let mut run = total - startup + 2.0 * settings.costs.cpu_operator * rows;
    let bytes = relation_byte_size(rows, width);
    if bytes > settings.work_mem_bytes {
        run += settings.costs.seq_page * (bytes / BLOCK).ceil();
    }
    (startup, startup + run)
}

/// `cost_sort` of a path's rows: the sort's startup and total cost.
pub(crate) fn sort_cost(path: &Path, settings: JoinSettings) -> (f64, f64) {
    let (startup, run) = super::paths::tuplesort_cost(path.rows, path.width, None, settings.costs);
    let startup = startup + path.total;
    (startup, startup + run)
}

/// `page_size`.
fn page_size(tuples: f64, width: f64) -> f64 {
    (relation_byte_size(tuples, width) / BLOCK).ceil()
}

/// What `ExecChooseHashTableSize` chooses for a table of `tuples` rows.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) struct HashSizing {
    pub(crate) buckets: u32,
    pub(crate) batches: u32,
    pub(crate) skew_mcvs: u32,
    /// `space_allowed`: the table's memory, its skew buckets' included.
    pub(crate) space_allowed: f64,
}

/// `ExecChooseHashTableSize` of a serial hash join with skew optimization.
pub(crate) fn choose_hash_table_size(
    tuples: f64,
    width: f64,
    settings: JoinSettings,
) -> HashSizing {
    /// `HJTUPLE_OVERHEAD` and `MAXALIGN(SizeofMinimalTupleHeader)`.
    const TUPLE_OVERHEAD: f64 = 16.0 + 16.0;
    const POINTER: f64 = 8.0;
    const MAX_ALLOC: f64 = 0x3fff_ffff as f64;
    let tuples = if tuples <= 0.0 { 1000.0 } else { tuples };
    let tuple_size = TUPLE_OVERHEAD + (width as u64).next_multiple_of(8) as f64;
    let inner_bytes = tuples * tuple_size;
    let mut table_bytes = settings.hash_mem_bytes.trunc();
    let mut space_allowed = table_bytes;
    // `bytes_per_mcv`: the tuple, eight bucket pointers, the skew bucket
    // number and the `HashSkewBucket`.
    let per_mcv = tuple_size + 8.0 * POINTER + 4.0 + 16.0;
    let mut skew_mcvs = ((table_bytes / per_mcv).trunc() * 2.0 / 100.0).trunc();
    if skew_mcvs > 0.0 {
        table_bytes -= skew_mcvs * per_mcv;
    }
    let mut max_pointers = (table_bytes / POINTER)
        .trunc()
        .min((MAX_ALLOC / POINTER).trunc());
    max_pointers = prev_power_of_two(max_pointers).min(f64::from(i32::MAX / 2 + 1));
    let buckets = (tuples / 1.0).ceil().min(max_pointers).max(1024.0);
    let mut buckets = next_power_of_two(buckets);
    let mut batches = 1.0;
    if inner_bytes + buckets * POINTER > table_bytes {
        let bucket_size = tuple_size + POINTER;
        let sized = if table_bytes <= bucket_size {
            1.0
        } else {
            next_power_of_two((table_bytes / bucket_size).trunc())
        };
        buckets = next_power_of_two(sized.min(max_pointers));
        let batch = (inner_bytes / (table_bytes - buckets * POINTER))
            .ceil()
            .min(max_pointers);
        batches = next_power_of_two(batch.max(2.0));
    }
    // Halving the batches while their file buffers outweigh the table
    // memory a halving doubles.
    while batches > 1.0 {
        if buckets > MAX_ALLOC / POINTER / 2.0 || batches < (space_allowed / BLOCK).trunc() {
            break;
        }
        buckets *= 2.0;
        skew_mcvs *= 2.0;
        space_allowed *= 2.0;
        batches /= 2.0;
    }
    HashSizing {
        buckets: buckets as u32,
        batches: batches as u32,
        skew_mcvs: skew_mcvs as u32,
        space_allowed,
    }
}

fn next_power_of_two(value: f64) -> f64 {
    (value.max(1.0) as u64).next_power_of_two() as f64
}

fn prev_power_of_two(value: f64) -> f64 {
    let value = value.max(1.0) as u64;
    (1u64 << value.ilog2()) as f64
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CostOrder {
    Better1,
    Better2,
    Equal,
    Different,
}

/// `compare_path_costs_fuzzily` where no relation considers startup cost
/// (a query without `LIMIT`).
fn compare_fuzzily(new: &Path, old: &Path, fuzz: f64) -> CostOrder {
    if new.disabled != old.disabled {
        return if new.disabled < old.disabled {
            CostOrder::Better1
        } else {
            CostOrder::Better2
        };
    }
    if new.total > old.total * fuzz {
        return CostOrder::Better2;
    }
    if old.total > new.total * fuzz {
        return CostOrder::Better1;
    }
    if new.startup > old.startup * fuzz {
        return CostOrder::Better2;
    }
    if old.startup > new.startup * fuzz {
        return CostOrder::Better1;
    }
    CostOrder::Equal
}

/// `compare_path_costs`: negative when `first` is cheaper.
fn compare_costs(first: &Path, second: &Path, startup: bool) -> i32 {
    if first.disabled != second.disabled {
        return if first.disabled < second.disabled {
            -1
        } else {
            1
        };
    }
    let order = if startup {
        [(first.startup, second.startup), (first.total, second.total)]
    } else {
        [(first.total, second.total), (first.startup, second.startup)]
    };
    for (left, right) in order {
        if left < right {
            return -1;
        }
        if left > right {
            return 1;
        }
    }
    0
}

/// `bms_subset_compare`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Subset {
    Equal,
    Subset1,
    Subset2,
    Different,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum KeysOrder {
    Better1,
    Better2,
    Equal,
    Different,
}

/// `compare_pathkeys`.
fn compare_keys(first: &Keys, second: &Keys) -> KeysOrder {
    let common = first.common_prefix(second);
    match (
        common == usize::from(first.len),
        common == usize::from(second.len),
    ) {
        (true, true) => KeysOrder::Equal,
        (true, false) => KeysOrder::Better2,
        (false, true) => KeysOrder::Better1,
        (false, false) => KeysOrder::Different,
    }
}

#[cfg(all(test, not(feature = "sql-fixture-suite")))]
mod tests {
    use super::*;

    fn settings() -> JoinSettings {
        JoinSettings {
            costs: Costs {
                seq_page: 1.0,
                random_page: 4.0,
                cpu_tuple: 0.01,
                cpu_index_tuple: 0.005,
                cpu_operator: 0.0025,
                effective_cache_size: 524_288.0,
            },
            work_mem_bytes: 4.0 * 1024.0 * 1024.0,
            hash_mem_bytes: 8.0 * 1024.0 * 1024.0,
            nestloop: true,
            hashjoin: true,
            mergejoin: true,
            material: true,
            sort: true,
            memoize: true,
        }
    }

    #[test]
    fn hash_table_sizes_follow_exec_choose_hash_table_size() {
        // 1000 rows of 12 bytes fit work_mem's doubled hash memory.
        assert_eq!(
            choose_hash_table_size(1000.0, 12.0, settings()),
            HashSizing {
                buckets: 1024,
                batches: 1,
                skew_mcvs: 1271,
                space_allowed: 8.0 * 1024.0 * 1024.0,
            }
        );
        assert_eq!(
            choose_hash_table_size(5000.0, 12.0, settings()).buckets,
            8192
        );
    }

    struct Uniform;

    impl JoinStatistics for Uniform {
        fn equality(&self, _: Member, _: Member) -> Result<f64, SqlError> {
            Ok(1.0 / 1000.0)
        }
        fn bucket(&self, _: Member, buckets: f64) -> Result<(f64, f64), SqlError> {
            Ok((0.0, (1.0 / 1000.0_f64).max(1.0 / buckets)))
        }
        fn merge_range(&self, _: Member, _: Member) -> Result<Option<[f64; 4]>, SqlError> {
            Ok(Some([0.0, 1.0, 0.0, 1.0]))
        }
        fn groups(&self, _: &[Member], calls: f64) -> Result<f64, SqlError> {
            Ok(calls)
        }
    }

    /// A sequential scan of `rows` rows over `total` cost.
    fn seq(rows: f64, total: f64) -> BasePath {
        BasePath {
            startup: 0.0,
            total,
            rows,
            disabled: 0,
            keys: Keys::NONE,
            param: 0,
            enforced: ClauseList::EMPTY,
            mark_restore: false,
            indexed_join_quals: false,
        }
    }

    #[test]
    fn two_relations_hash_the_smaller() {
        // a(id) 1000 rows of 12 bytes, b(id) 2000 rows of 8 bytes, as
        // `EXPLAIN SELECT * FROM a JOIN b ON a.id = b.id` plans them:
        // Hash Join (cost=28.50..85.00 rows=2000 width=20).
        let mut budget = crate::mem::budget::Budget::new(1 << 21);
        let arena = Arena::new(&mut budget, "join search", 1 << 20).unwrap();
        let mut class = EquivalenceClass {
            members: [Member { rel: 0, column: 0 }; MAX_MEMBERS],
            count: 2,
            has_const: false,
        };
        class.members[1] = Member { rel: 1, column: 0 };
        let problem = JoinProblem {
            rels: &[
                BaseRel {
                    rows: 1000.0,
                    width: 12.0,
                    paths: &[seq(1000.0, 16.0)],
                    unique: &[],
                    fixed: 0,
                },
                BaseRel {
                    rows: 2000.0,
                    width: 8.0,
                    paths: &[seq(2000.0, 29.0)],
                    unique: &[],
                    fixed: 0,
                },
            ],
            classes: &[class],
            clauses: &[Clause {
                form: ClauseForm::Equality {
                    class: 0,
                    left: 0,
                    right: 1,
                },
                relids: 3,
                cost: 0.0025,
                selectivity: None,
                source: Some(0),
            }],
            needed: &[
                NeededColumn {
                    rel: 0,
                    column: 0,
                    width: 12.0,
                    needed_by: 2,
                    output: true,
                },
                NeededColumn {
                    rel: 1,
                    column: 0,
                    width: 8.0,
                    needed_by: 1,
                    output: true,
                },
            ],
            settings: settings(),
            foreign_keys: &[],
        };
        let plan = search(&problem, &Uniform, &arena).unwrap().unwrap();
        let top = plan.path(plan.top);
        let PathKind::HashJoin { outer, inner, .. } = top.kind else {
            panic!("{top:?}");
        };
        assert_eq!(plan.path(outer).relids, 2);
        assert_eq!(plan.path(inner).relids, 1);
        assert_eq!((top.startup, top.total, top.rows), (28.5, 85.0, 2000.0));
    }
}
