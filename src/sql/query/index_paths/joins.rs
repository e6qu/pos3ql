//! PostgreSQL 18's join search over plain relations: `standard_join_search`
//! and `join_search_one_level` with the outer joins' ordering limits
//! (`join_is_legal`, `joinrels.c`), `make_join_rel` and
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
/// Join clauses of one join or relation, and sort keys of one path.
pub(crate) const MAX_JOIN_CLAUSES: usize = 32;
pub(crate) const MAX_KEYS: usize = 8;
/// Paths kept per relation by `add_path`.
const MAX_REL_PATHS: usize = 32;

/// A set of base relations, by bit, and of the outer joins above them
/// (PostgreSQL's join relids), each at [`OJ_SHIFT`] past its number.
pub(crate) type Relids = u32;
/// The first outer join's bit.
pub(crate) const OJ_SHIFT: u32 = 16;
/// The base relations' bits of a [`Relids`].
pub(crate) const BASE_RELIDS: Relids = (1 << OJ_SHIFT) - 1;

/// `STD_FUZZ_FACTOR`.
const FUZZ: f64 = 1.01;
/// `BLCKSZ`.
const BLOCK: f64 = 8192.0;

/// A column of a base relation that an equivalence class equates, with
/// the outer joins that null it where it is read (`varnullingrels`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Member {
    pub(crate) rel: u8,
    pub(crate) column: u16,
    pub(crate) nulling: Relids,
}

impl Member {
    /// The column as its own relation's scan reads it.
    pub(crate) const fn plain(rel: u8, column: u16) -> Self {
        Self {
            rel,
            column,
            nulling: 0,
        }
    }

    /// `em_relids`.
    pub(crate) const fn relids(&self) -> Relids {
        1 << self.rel | self.nulling
    }
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

    /// `ec_relids`.
    pub(crate) fn relids(&self) -> Relids {
        self.members()
            .iter()
            .fold(0, |relids, member| relids | member.relids())
    }
}

/// How a join clause compares its sides.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ClauseForm {
    /// `left = right` of two members of an equivalence class: mergeable
    /// and hashable.
    Equality { class: u8, left: u8, right: u8 },
    /// `left = right` of two columns no class holds (an outer join's
    /// clause): mergeable and hashable, each side sorted by its own class
    /// (`left_ec`, `right_ec`).
    Join {
        left: Member,
        right: Member,
        left_class: u8,
        right_class: u8,
    },
    /// Any other condition, whose selectivity the caller computed.
    Other,
}

/// The shape of a condition other than a mergeable equality, as far as a
/// semi or anti join's estimate of it differs from an inner join's.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum OtherShape {
    /// Estimated alike for every join.
    Plain,
    /// `left <> right` of columns with these NULL fractions: `neqjoinsel`
    /// of a semi or anti join is the outer column's non-NULL fraction.
    Inequality { left_nulls: f64, right_nulls: f64 },
    /// An `OR` of conditions, whose semi join estimate the model leaves.
    Composite,
}

/// Which copy of an outer join's clause a clause is, when the join
/// commutes with another (`has_clone`, `is_clone`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum CloneKind {
    None,
    Has,
    Is,
}

/// A join clause (`RestrictInfo`).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Clause {
    pub(crate) form: ClauseForm,
    /// `clause_relids`: the relations it reads and the outer joins that
    /// null them.
    pub(crate) relids: Relids,
    /// `required_relids`: where it can be evaluated.
    pub(crate) required: Relids,
    /// `is_pushed_down`: a filter of the rows the join returns rather than
    /// the condition an outer join matches by.
    pub(crate) pushed_down: bool,
    pub(crate) clone: CloneKind,
    pub(crate) incompatible: Relids,
    /// `outer_relids`: an outer join clause's preserved side, which the
    /// clause cannot move into.
    pub(crate) outer_relids: Relids,
    /// The columns another condition compares, `left op right`, whose
    /// outer side a `Memoize` cache keys by (`check_memoize_hashability`).
    pub(crate) operands: Option<(Member, Member)>,
    /// What a semi or anti join estimates another condition by.
    pub(crate) other: OtherShape,
    /// `cost_qual_eval`'s per-tuple cost.
    pub(crate) cost: f64,
    /// Its selectivity for an inner join (`norm_selec`), once computed.
    pub(crate) selectivity: Option<f64>,
    /// Its selectivity for any other join type (`outer_selec`), once
    /// computed: PostgreSQL keeps one for them all.
    pub(crate) outer_selectivity: Option<f64>,
    /// The caller's clause this one is, or `None` for one an equivalence
    /// class implied.
    pub(crate) source: Option<u16>,
}

impl Clause {
    /// `RINFO_IS_PUSHED_DOWN` at a join of `relids`.
    pub(crate) fn is_pushed_down(&self, relids: Relids) -> bool {
        self.pushed_down || self.required & !relids != 0
    }

    /// A clause of some relation's `joininfo`: one no class absorbed,
    /// required at more than one relation or outer join.
    fn in_joininfo(&self) -> bool {
        !matches!(self.form, ClauseForm::Equality { .. }) && self.required.count_ones() > 1
    }

    /// A clause of the `joininfo` of base relation `relids`.
    pub(crate) fn in_joininfo_of(&self, relids: Relids) -> bool {
        self.in_joininfo() && self.required & relids != 0
    }
}

/// The kind of an outer, semi or anti join (`SpecialJoinInfo.jointype`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum SpecialKind {
    Left,
    Full,
    Semi,
    Anti,
}

/// An outer, semi or anti join of the query and its ordering limits
/// (`SpecialJoinInfo`).
#[derive(Clone, Copy, Debug)]
pub(crate) struct SpecialJoin {
    pub(crate) kind: SpecialKind,
    pub(crate) min_left: Relids,
    pub(crate) min_right: Relids,
    pub(crate) syn_left: Relids,
    pub(crate) syn_right: Relids,
    /// The join's own bit (`ojrelid`), 0 for a semi join.
    pub(crate) oj: Relids,
    pub(crate) lhs_strict: bool,
    pub(crate) commute_above_l: Relids,
    pub(crate) commute_above_r: Relids,
    pub(crate) commute_below_l: Relids,
    pub(crate) commute_below_r: Relids,
    /// A semi join's inner columns its equalities compare, by which its
    /// inner relation can be made unique (`semi_rhs_exprs`), and whether
    /// sorting or hashing can (`semi_can_btree`, `semi_can_hash`).
    pub(crate) semi_rhs: [Member; MAX_KEYS],
    pub(crate) semi_count: u8,
    pub(crate) semi_can_btree: bool,
    pub(crate) semi_can_hash: bool,
}

/// The join type of a join path (`JoinType`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum JoinType {
    Inner,
    Left,
    Full,
    Semi,
    Anti,
    Right,
    RightSemi,
    RightAnti,
    /// An inner join of a semi join's unique-ified inner relation, or of
    /// its unique-ified outer one (`JOIN_UNIQUE_INNER`, `JOIN_UNIQUE_OUTER`):
    /// the paths are inner joins.
    UniqueInner,
    UniqueOuter,
}

impl JoinType {
    /// `IS_OUTER_JOIN`.
    pub(crate) const fn is_outer(self) -> bool {
        matches!(
            self,
            Self::Left | Self::Full | Self::Anti | Self::Right | Self::RightAnti
        )
    }

    /// Whether the join returns its outer input's rows without a match.
    pub(crate) const fn fills_outer(self) -> bool {
        matches!(self, Self::Left | Self::Full | Self::Anti)
    }

    /// Whether the join returns its inner input's rows without a match.
    pub(crate) const fn fills_inner(self) -> bool {
        matches!(self, Self::Right | Self::Full | Self::RightAnti)
    }
}

/// One join problem of the query (`make_rel_from_joinlist`): the
/// relations it joins, a lower problem's joined relations as one item.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Subproblem {
    pub(crate) items: [Relids; MAX_JOIN_RELS],
    pub(crate) count: u8,
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
    /// `eqjoinsel_semi` of `outer = inner` for a semi or anti join whose
    /// inner input has `inner_rows` rows: the fraction of outer rows with a
    /// match.
    fn semi(&self, outer: Member, inner: Member, inner_rows: f64) -> Result<f64, SqlError>;
    /// `estimate_num_groups` of the columns `members` over `calls` rows;
    /// `calls` when a column's distinct count is a default.
    fn groups(&self, members: &[Member], calls: f64) -> Result<f64, SqlError>;
    /// `estimate_num_groups` of `members` over `rows` input rows.
    fn num_groups(&self, members: &[Member], rows: f64) -> Result<f64, SqlError>;
    /// `estimate_hash_bucket_stats` of `member` over `buckets`: its most
    /// common value's frequency and its bucket size fraction.
    fn bucket(&self, member: Member, buckets: f64) -> Result<(f64, f64), SqlError>;
    /// `mergejoinscansel` of `left = right` merged in the given direction:
    /// `(leftstart, leftend, rightstart, rightend)`.
    /// `None` when PostgreSQL's estimate is outside the model.
    fn merge_range(
        &self,
        left: Member,
        right: Member,
        descending: bool,
        nulls_first: bool,
    ) -> Result<Option<[f64; 4]>, SqlError>;
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
    pub(crate) incremental_sort: bool,
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

/// A pathkey's equivalence class bits; the others are its direction.
pub(crate) const KEY_CLASS: u8 = 0x3f;
/// A pathkey sorting its class descending.
pub(crate) const KEY_DESCENDING: u8 = 0x40;
/// A pathkey placing NULLs first.
pub(crate) const KEY_NULLS_FIRST: u8 = 0x80;

/// A path's sort order: pathkeys, each an equivalence class with
/// [`KEY_DESCENDING`] and [`KEY_NULLS_FIRST`] flags; without flags it is
/// ascending with NULLs last, as merge joins sort.
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

    /// Its first `len` keys.
    pub(crate) fn truncated(mut self, len: usize) -> Self {
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
    /// A dummy relation's path: a `Result` whose constant false
    /// condition returns no rows (`set_dummy_rel_pathlist`).
    Dummy,
    Scan {
        rel: u8,
        /// Its position in [`BaseRel::paths`].
        path: u16,
    },
    Material {
        input: u32,
    },
    /// A semi join's inner relation made unique by its compared columns
    /// (`create_unique_path`): sorted and deduplicated, hashed, or already
    /// unique by an index (`UNIQUE_PATH_NOOP`, which plans no node).
    Unique {
        input: u32,
        keys: [Member; MAX_KEYS],
        key_count: u8,
        method: UniqueMethod,
    },
    NestLoop {
        jointype: JoinType,
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
        /// `binary_mode`: a moved clause no hash equality applies, so keys
        /// match by their bytes.
        binary: bool,
    },
    HashJoin {
        jointype: JoinType,
        outer: u32,
        inner: u32,
        clauses: ClauseList,
        hash: ClauseList,
        buckets: u32,
        batches: u32,
    },
    MergeJoin {
        jointype: JoinType,
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
        /// Each merge clause's sort direction, the outer pathkey's of its
        /// class (`mergeStrategies`, `mergeNullsFirst`): bit `i` of each
        /// for merge clause `i`.
        descending: u32,
        nulls_first: u32,
    },
}

/// How a unique-ified relation is made unique (`UniquePathMethod`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum UniqueMethod {
    Noop,
    Sort,
    Hash,
}

/// A path of a base or join relation.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Path {
    pub(crate) kind: PathKind,
    /// Its relation's relids, the outer joins it performs included.
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
    relids: Relids,
    /// The clauses joining it to relations outside it (`joininfo`).
    joininfo: ClauseList,
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
    /// `cheapest_unique_path`, once a semi join asked for it.
    unique: Option<Option<u32>>,
}

/// The chosen plan: every path the search built, and the cheapest of the
/// relation joining them all.
pub(crate) struct JoinPlan<'a> {
    pub(crate) paths: &'a [Path],
    pub(crate) clauses: &'a [Clause],
    pub(crate) classes: &'a [EquivalenceClass],
    /// The join's chosen path, below [`Self::upper`].
    pub(crate) top: u32,
    pub(crate) upper: UpperPlan,
    pub(crate) settings: JoinSettings,
    /// For each join relation, by its base relations, the pair that built
    /// it.
    pub(crate) firsts: &'a [(Relids, Relids)],
    pub(crate) needed: &'a [NeededColumn],
    /// The relations of removed joins, which the plan binds to NULLs.
    pub(crate) removed: Relids,
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
        let (outer, inner) = self.firsts[(part & BASE_RELIDS) as usize];
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
/// pair an equivalence class or a join clause equates
/// (`match_foreign_keys_to_quals`).
#[derive(Clone, Copy, Debug)]
pub(crate) struct ForeignKeyJoin {
    pub(crate) referencing: u8,
    pub(crate) referenced: u8,
    /// Each key column pair's class, or [`NO_KEY_CLASS`] for a pair join
    /// clauses equate.
    pub(crate) classes: [u8; MAX_KEYS],
    pub(crate) count: u8,
    /// The join clauses equating the pairs no class holds (`rinfos`).
    pub(crate) clauses: ClauseList,
    /// The referenced table's rows (`ref_rel->tuples`, at least one) and
    /// its estimated rows after its restrictions (`ref_rel->rows`).
    pub(crate) referenced_tuples: f64,
    pub(crate) referenced_rows: f64,
}

/// A foreign key column pair no class holds.
pub(crate) const NO_KEY_CLASS: u8 = u8::MAX;

/// What the query does with the joined rows that chooses among the
/// join's paths (`grouping_planner` of a query without grouping).
#[derive(Clone, Copy, Debug)]
pub(crate) struct UpperProblem {
    /// `sort_pathkeys`, which are the query's `query_pathkeys`.
    pub(crate) sort: Keys,
    /// The `LIMIT`'s rows, when not `ALL` or NULL, and the `OFFSET`'s, when
    /// either is given as a constant.
    pub(crate) limit: Option<(Option<f64>, f64)>,
    /// The width of the query's target list, its ordering columns
    /// included (`apply_scanjoin_target_to_paths`).
    pub(crate) width: f64,
    /// The per-row cost of the target list's expressions, which the
    /// joined relation's paths take on (`create_projection_path`).
    pub(crate) target_cost: f64,
    /// The query's `GROUP BY`, whose pathkeys `sort` then holds as the
    /// query's pathkeys.
    pub(crate) group: Option<GroupProblem>,
}

/// A `GROUP BY` of plain columns, or the one group of a query's plain
/// aggregates, above the join (`create_grouping_paths`).
#[derive(Clone, Copy, Debug)]
pub(crate) struct GroupProblem {
    /// `group_pathkeys`, in `processed_groupClause` order; none for plain
    /// aggregation (`AGG_PLAIN`).
    pub(crate) keys: Keys,
    /// `sort_pathkeys` of the query's `ORDER BY` of the groups.
    pub(crate) sort: Keys,
    /// Each key's grouping column, which `get_number_of_groups` counts the
    /// groups of.
    pub(crate) members: [Member; MAX_KEYS],
    pub(crate) aggregates: super::aggregate::AggregateCosts,
    /// The width of the joined rows the aggregate reads and of the groups
    /// it returns.
    pub(crate) input_width: f64,
    pub(crate) output_width: f64,
    /// Every grouping column's type hashes (`GROUPING_CAN_USE_HASH`).
    pub(crate) hashable: bool,
    pub(crate) hashagg: bool,
    pub(crate) group_by_reordering: bool,
    /// A `DISTINCT` (`create_final_distinct_paths`): a `Unique` over the
    /// rows sorted by its keys rather than a `GroupAggregate`, and a
    /// `HashAggregate` only with `enable_hashagg` on;
    /// `group_by_reordering` is then `enable_distinct_reordering`.
    pub(crate) distinct: bool,
    /// The `HAVING` qualification's cost and selectivity.
    pub(crate) having: Option<super::aggregate::QualCosts>,
    /// The per-group cost of the grouping's target list.
    pub(crate) target_cost: f64,
}

/// The grouping above the join that `add_paths_to_grouping_rel` chose.
#[derive(Clone, Copy, Debug)]
pub(crate) struct GroupPlan {
    /// A `HashAggregate` rather than a `GroupAggregate`.
    pub(crate) hashed: bool,
    /// The pathkeys a `GroupAggregate` groups by, its `Group Key` order.
    pub(crate) keys: Keys,
    /// The sort of the joined rows below a `GroupAggregate`: its startup
    /// and total cost and the keys the rows are already sorted by.
    pub(crate) input_sort: Option<(f64, f64, u8)>,
    pub(crate) startup: f64,
    pub(crate) total: f64,
    pub(crate) rows: f64,
    pub(crate) width: f64,
    /// The groups estimated before any `HAVING` (`numGroups`).
    pub(crate) groups: f64,
    /// The join's rows as the aggregate reads them.
    pub(crate) input: AggregateInput,
}

/// The rows and cost of an aggregate's input.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct AggregateInput {
    pub(crate) rows: f64,
}

/// The query's nodes above the join, as `create_ordered_paths` and
/// `create_limit_path` chose them.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct UpperPlan {
    /// A `Sort` of the joined rows, or an `Incremental Sort` of rows sorted
    /// by its first keys: its startup and total cost and those keys.
    pub(crate) sort: Option<(f64, f64, u8)>,
    /// A `Limit`: its startup and total cost and its rows.
    pub(crate) limit: Option<(f64, f64, f64)>,
    /// The join path's total cost with its target list's.
    pub(crate) top_total: f64,
    /// The grouping between the join and the sort, when the query groups.
    pub(crate) group: Option<GroupPlan>,
}

/// The join problem.
pub(crate) struct JoinProblem<'p> {
    pub(crate) rels: &'p [BaseRel<'p>],
    pub(crate) classes: &'p [EquivalenceClass],
    /// The outer, semi and anti joins (`join_info_list`).
    pub(crate) specials: &'p [SpecialJoin],
    /// The join problems, lower ones first; the last joins them all.
    pub(crate) subproblems: &'p [Subproblem],
    /// The query's join clauses in the order PostgreSQL distributes them,
    /// equalities of class members and other clauses above one relation
    /// included, then the clauses its base relations' index paths derived
    /// (`source` `None`).
    pub(crate) clauses: &'p [Clause],
    pub(crate) needed: &'p [NeededColumn],
    pub(crate) settings: JoinSettings,
    /// The query's foreign keys, as `root->fkey_list` holds them.
    pub(crate) foreign_keys: &'p [ForeignKeyJoin],
    /// The relations of removed joins (`remove_useless_joins`), which no
    /// join reads.
    pub(crate) removed: Relids,
    pub(crate) upper: UpperProblem,
    /// A class equates two constants in the query's join domain: the
    /// joined relation is dummy.
    pub(crate) dummy: bool,
    /// The base relations a class equating two constants inside an outer
    /// join empties: one dummy relation, or the join of several, whose
    /// restriction list holds the constant false condition.
    pub(crate) false_relids: Relids,
}

struct Search<'p, 'a, 's> {
    problem: &'p JoinProblem<'p>,
    /// `join_info_list` after `reduce_unique_semijoins`.
    specials: &'a [SpecialJoin],
    statistics: &'s dyn JoinStatistics,
    arena: &'a Arena,
    clauses: ArenaList<'a, Clause>,
    paths: ArenaList<'a, Path>,
    /// Relations by their base relations; `None` until built.
    rels: &'a mut [Option<Rel>],
    /// The joinrels of each level of the current problem in the order they
    /// were built; its items are the first level (`initial_rels`).
    levels: [ArenaList<'a, Relids>; MAX_JOIN_RELS + 1],
    /// A shape outside the model met while generating paths.
    outside: Option<Outside>,
    /// Each Memoize path's `cost_memoize_rescan`.
    memoize_rescans: ArenaList<'a, (u32, f64, f64)>,
    /// Each parameterized join's `ppi_rows`, by its relations and
    /// parameterization.
    param_rows: ArenaList<'a, (Relids, Relids, f64)>,
    /// Each clause side's `estimate_hash_bucket_stats`, as its first
    /// estimate cached it on the clause (`left_bucketsize`,
    /// `right_bucketsize` and their most common value frequencies).
    buckets: [[Option<(f64, f64)>; 2]; MAX_CLAUSES],
    /// `all_baserels`.
    all_baserels: Relids,
    /// `consider_startup` of every relation: the query has a `LIMIT`
    /// (`root->tuple_fraction > 0`).
    consider_startup: bool,
}

/// Plans the join of `problem`'s relations as `make_rel_from_joinlist` and
/// `standard_join_search` do.
pub(crate) fn search<'a>(
    problem: &JoinProblem<'_>,
    statistics: &dyn JoinStatistics,
    arena: &'a Arena,
) -> Result<Result<JoinPlan<'a>, Outside>, SqlError> {
    let n = problem.rels.len();
    if !(2..=MAX_JOIN_RELS).contains(&n) {
        return Ok(Err("a join of more relations than join_collapse_limit"));
    }
    if problem.clauses.len() > MAX_CLAUSES {
        return Err(too_many());
    }
    let full = arena_slice(arena, 1usize << n, |_| None::<Rel>)?;
    let mut search = Search {
        problem,
        specials: &[],
        statistics,
        arena,
        clauses: ArenaList::new(arena),
        paths: ArenaList::new(arena),
        rels: full,
        levels: core::array::from_fn(|_| ArenaList::new(arena)),
        outside: None,
        memoize_rescans: ArenaList::new(arena),
        param_rows: ArenaList::new(arena),
        buckets: [[None; 2]; MAX_CLAUSES],
        all_baserels: ((1 << n) - 1) & !problem.removed,
        consider_startup: problem
            .upper
            .limit
            .is_some_and(|(count, _)| count.is_some()),
    };
    for clause in problem.clauses {
        search.clauses.push(*clause).map_err(|_| arena_full())?;
    }
    for (index, rel) in problem.rels.iter().enumerate() {
        let relids = 1 << index;
        // `distribute_restrictinfo_to_rels`: a clause required at several
        // relations or outer joins joins each of its base relations.
        let mut joininfo = ClauseList::EMPTY;
        for (id, clause) in problem.clauses.iter().enumerate() {
            if clause.in_joininfo() && clause.required & relids != 0 {
                joininfo.push(id as u8).ok_or_else(too_many)?;
            }
        }
        search.rels[relids as usize] = Some(Rel {
            relids,
            joininfo,
            rows: rel.rows,
            width: rel.width,
            paths: [0; MAX_REL_PATHS],
            count: 0,
            cheapest_total: 0,
            cheapest_startup: 0,
            parameterized: [0; MAX_REL_PATHS],
            parameterized_count: 0,
            first: (relids, 0),
            unique: None,
        });
        // A relation its constant false condition empties is dummy
        // (`relation_excluded_by_constraints`, `set_dummy_rel_pathlist`).
        if problem.false_relids == relids {
            search.mark_dummy(relids)?;
            continue;
        }
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
    }
    search.reduce_unique_semijoins()?;
    if problem.dummy {
        // The final relation is the dummy one the query's constant false
        // condition leaves.
        let relids = search.all_baserels;
        let id = search.push_path(Path {
            kind: PathKind::Dummy,
            relids,
            rows: 0.0,
            width: 0.0,
            startup: 0.0,
            total: 0.0,
            disabled: 0,
            keys: Keys::NONE,
            param: 0,
        })?;
        let mut paths = [0; MAX_REL_PATHS];
        paths[0] = id;
        search.rels[relids as usize] = Some(Rel {
            relids,
            joininfo: ClauseList::EMPTY,
            rows: 0.0,
            width: 0.0,
            paths,
            count: 1,
            cheapest_total: id,
            cheapest_startup: id,
            parameterized: paths,
            parameterized_count: 1,
            first: (relids, 0),
            unique: None,
        });
    }
    for subproblem in problem.subproblems.iter().filter(|_| !problem.dummy) {
        let items = &subproblem.items[..usize::from(subproblem.count)];
        if items.len() < 2 {
            continue;
        }
        search.levels = core::array::from_fn(|_| ArenaList::new(arena));
        for &item in items {
            // A lower problem's relation, with the outer joins it performs.
            let relids = search.rel(item).relids;
            search.levels[1].push(relids).map_err(|_| arena_full())?;
        }
        for level in 2..=items.len() {
            search.one_level(level)?;
            if let Some(outside) = search.outside {
                return Ok(Err(outside));
            }
            for at in 0..search.levels[level].len() {
                let relids = search.levels[level].borrowed_slice()[at];
                search.set_cheapest(relids);
            }
        }
        if search.levels[items.len()].len() != 1 {
            return Ok(Err("a join search that builds no join"));
        }
    }
    let all = search.all_baserels as usize;
    let Some(top) = search.rels[all] else {
        return Ok(Err("a join the search does not reach"));
    };
    let (top, upper) = search.upper_paths(&top)?;
    Ok(Ok(JoinPlan {
        paths: search.paths.as_slice(),
        clauses: search.clauses.as_slice(),
        classes: arena
            .alloc_slice_copy(problem.classes)
            .map_err(|_| arena_full())?,
        top,
        upper,
        settings: problem.settings,
        firsts: arena
            .alloc_slice_with(search.rels.len(), |relids| {
                search.rels[relids].map_or((0, 0), |rel| rel.first)
            })
            .map_err(|_| arena_full())?,
        needed: arena
            .alloc_slice_copy(problem.needed)
            .map_err(|_| arena_full())?,
        removed: problem.removed,
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
    jointype: JoinType,
    /// The outer, semi or anti join it performs, `None` for an inner join.
    special: Option<u8>,
    restrict: ClauseList,
    /// `select_mergejoin_clauses`' mergeable clauses, in restriction order.
    mergeable: ClauseList,
    /// `innerrel_is_unique`, and then `compute_semi_anti_join_factors`:
    /// the fraction of outer rows with a match and the matches each has.
    inner_unique: bool,
    match_fraction: f64,
    match_count: f64,
    /// `param_source_rels`.
    param_source: Relids,
}

impl JoinInputs {
    /// The join type of the paths: a unique-ified relation's join is an
    /// inner join.
    fn path_jointype(&self) -> JoinType {
        match self.jointype {
            JoinType::UniqueInner | JoinType::UniqueOuter => JoinType::Inner,
            jointype => jointype,
        }
    }

    /// The executor stops at an outer row's first match.
    fn first_match(&self) -> bool {
        matches!(self.jointype, JoinType::Semi | JoinType::Anti) || self.inner_unique
    }
}

/// Which selectivity cache a clause's estimate uses, and how an equality
/// is estimated when the cache is empty (`clause_selectivity_ext` with
/// `jointype` and `sjinfo`).
#[derive(Clone, Copy)]
enum Selectivity {
    /// `JOIN_INNER`: `norm_selec`.
    Inner,
    /// Any other join type: `outer_selec`, estimated with the join's
    /// `SpecialJoinInfo` (`None` for an inner join's).
    Outer(Option<u8>),
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

    /// Whether a clause list holds clause `id` or a copy of it: the copies
    /// of a commuting outer join's clause share its `rinfo_serial`.
    fn holds_serial(&self, list: &ClauseList, id: u8) -> bool {
        let source = self.clause(id).source;
        list.as_slice()
            .iter()
            .any(|&held| held == id || (source.is_some() && self.clause(held).source == source))
    }

    fn rel(&self, relids: Relids) -> Rel {
        self.rels[(relids & BASE_RELIDS) as usize].expect("the relation was built")
    }

    fn special(&self, id: u8) -> &SpecialJoin {
        &self.specials[usize::from(id)]
    }

    /// The sides of a mergeable clause: its left and right members and
    /// their classes (`left_ec`, `right_ec`).
    fn sides(&self, id: u8) -> Option<(Member, Member, u8, u8)> {
        match self.clause(id).form {
            ClauseForm::Equality { class, left, right } => {
                let members = self.problem.classes[usize::from(class)].members();
                Some((
                    members[usize::from(left)],
                    members[usize::from(right)],
                    class,
                    class,
                ))
            }
            ClauseForm::Join {
                left,
                right,
                left_class,
                right_class,
            } => Some((left, right, left_class, right_class)),
            ClauseForm::Other => None,
        }
    }

    /// `clause_sides_match_join`: whether the clause's left side is the
    /// outer relation's, `None` when its sides are not the two inputs'.
    fn outer_is_left(&self, id: u8, outer: Relids, inner: Relids) -> Option<bool> {
        let (left, right, ..) = self.sides(id)?;
        if left.relids() & !outer == 0 && right.relids() & !inner == 0 {
            Some(true)
        } else if right.relids() & !outer == 0 && left.relids() & !inner == 0 {
            Some(false)
        } else {
            None
        }
    }

    /// The class of a merge clause's outer side, and of its inner side.
    fn merge_classes(&self, id: u8, outer: Relids, inner: Relids) -> (u8, u8) {
        let (_, _, left, right) = self.sides(id).expect("a merge clause");
        if self.outer_is_left(id, outer, inner) == Some(true) {
            (left, right)
        } else {
            (right, left)
        }
    }

    /// `reduce_unique_semijoins`: a semi join whose inner relation is one
    /// base relation unique for its clauses with the outer relations is an
    /// inner join.
    fn reduce_unique_semijoins(&mut self) -> Result<(), SqlError> {
        let held = self.problem.specials;
        let kept = self
            .arena
            .alloc_slice_with(held.len(), |at| held[at])
            .map_err(|_| arena_full())?;
        let mut count = 0;
        for special in held {
            let inner = special.min_right;
            let reduced = special.kind == SpecialKind::Semi
                && inner.count_ones() == 1
                && inner & !BASE_RELIDS == 0
                && !self.problem.rels[inner.trailing_zeros() as usize]
                    .unique
                    .is_empty()
                && {
                    let joined = special.min_left | inner;
                    let mut restrict = ClauseList::EMPTY;
                    for (class_id, class) in self.problem.classes.iter().enumerate() {
                        if class.has_const || class.count <= 1 {
                            continue;
                        }
                        let members = class.members();
                        let first = |side: Relids| {
                            members.iter().position(|member| {
                                member.relids() & !joined == 0 && member.relids() & !side == 0
                            })
                        };
                        if let (Some(outer), Some(within)) = (first(special.min_left), first(inner))
                        {
                            let id =
                                self.class_clause(class_id as u8, outer as u8, within as u8)?;
                            restrict.push(id).ok_or_else(too_many)?;
                        }
                    }
                    for &id in self.rel(inner).joininfo.as_slice() {
                        restrict.push(id).ok_or_else(too_many)?;
                    }
                    self.inner_unique(joined, special.min_left, inner, JoinType::Semi, &restrict)
                };
            if !reduced {
                kept[count] = *special;
                count += 1;
            }
        }
        self.specials = &kept[..count];
        Ok(())
    }

    /// `has_relevant_eclass_joinclause`.
    fn has_eclass_joins(&self, relids: Relids) -> bool {
        self.problem.classes.iter().any(|class| {
            class.count > 1
                && class.relids() & relids & BASE_RELIDS != 0
                && class.relids() & !relids != 0
        })
    }

    /// `old_rel->joininfo != NIL || old_rel->has_eclass_joins ||
    /// has_join_restriction(old_rel)`: a class with a constant counts,
    /// though it derives no join clause.
    fn has_joins(&self, relids: Relids) -> bool {
        self.rel(relids).joininfo.len != 0
            || self.has_eclass_joins(relids)
            || self.has_join_restriction(relids)
    }

    /// `have_relevant_joinclause`.
    fn relevant(&self, left: Relids, right: Relids) -> bool {
        let (one, two) = (self.rel(left), self.rel(right));
        let (joininfo, other) = if one.joininfo.len <= two.joininfo.len {
            (one.joininfo, right)
        } else {
            (two.joininfo, left)
        };
        if joininfo
            .as_slice()
            .iter()
            .any(|&id| self.clause(id).required & other != 0)
        {
            return true;
        }
        self.has_eclass_joins(left)
            && self.has_eclass_joins(right)
            && self.problem.classes.iter().any(|class| {
                class.count > 1
                    && class.relids() & left & BASE_RELIDS != 0
                    && class.relids() & right & BASE_RELIDS != 0
            })
    }

    /// `has_join_restriction`.
    fn has_join_restriction(&self, relids: Relids) -> bool {
        self.specials.iter().any(|special| {
            special.kind != SpecialKind::Full
                && !(special.min_left & !relids == 0 && special.min_right & !relids == 0)
                && (special.min_left & relids != 0 || special.min_right & relids != 0)
        })
    }

    /// `have_join_order_restriction`.
    fn order_restriction(&mut self, left: Relids, right: Relids) -> Result<bool, SqlError> {
        let restricted = self.specials.iter().any(|special| {
            special.kind != SpecialKind::Full
                && ((special.min_left & !left == 0 && special.min_right & !right == 0)
                    || (special.min_left & !right == 0 && special.min_right & !left == 0)
                    || (special.min_right & left != 0 && special.min_right & right != 0)
                    || (special.min_left & left != 0 && special.min_left & right != 0))
        });
        if !restricted {
            return Ok(false);
        }
        Ok(!(self.has_legal_joinclause(left)? || self.has_legal_joinclause(right)?))
    }

    /// `has_legal_joinclause`.
    fn has_legal_joinclause(&mut self, relids: Relids) -> Result<bool, SqlError> {
        let initial = self.levels[1].as_slice();
        for &other in initial {
            if other & relids != 0 || !self.relevant(relids, other) {
                continue;
            }
            if self.join_is_legal(relids, other, relids | other)?.is_some() {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// `join_is_legal`: the special join the pair performs and whether it
    /// is reversed, `Some((None, false))` for an inner join, `None` when
    /// the pair cannot join.
    #[allow(clippy::type_complexity)]
    fn join_is_legal(
        &mut self,
        rel1: Relids,
        rel2: Relids,
        joined: Relids,
    ) -> Result<Option<(Option<u8>, bool)>, SqlError> {
        let mut matched: Option<u8> = None;
        let mut reversed = false;
        let mut must_be_left = false;
        let subset = |small: Relids, big: Relids| small & !big == 0;
        for (id, special) in self.specials.iter().enumerate() {
            let id = id as u8;
            if special.min_right & joined == 0 || subset(joined, special.min_right) {
                continue;
            }
            if (subset(special.min_left, rel1) && subset(special.min_right, rel1))
                || (subset(special.min_left, rel2) && subset(special.min_right, rel2))
            {
                continue;
            }
            if special.kind == SpecialKind::Semi
                && ((subset(special.syn_right, rel1) && special.syn_right != rel1)
                    || (subset(special.syn_right, rel2) && special.syn_right != rel2))
            {
                continue;
            }
            if subset(special.min_left, rel1) && subset(special.min_right, rel2) {
                if matched.is_some() {
                    return Ok(None);
                }
                matched = Some(id);
                reversed = false;
            } else if subset(special.min_left, rel2) && subset(special.min_right, rel1) {
                if matched.is_some() {
                    return Ok(None);
                }
                matched = Some(id);
                reversed = true;
            } else if special.kind == SpecialKind::Semi
                && special.syn_right == rel2
                && self.unique_path(rel2, id)?.is_some()
            {
                if matched.is_some() {
                    return Ok(None);
                }
                matched = Some(id);
                reversed = false;
            } else if special.kind == SpecialKind::Semi
                && special.syn_right == rel1
                && self.unique_path(rel1, id)?.is_some()
            {
                if matched.is_some() {
                    return Ok(None);
                }
                matched = Some(id);
                reversed = true;
            } else {
                if special.min_right & rel1 != 0 && special.min_right & rel2 != 0 {
                    continue;
                }
                if special.kind != SpecialKind::Left || joined & special.min_left != 0 {
                    return Ok(None);
                }
                must_be_left = true;
            }
        }
        if must_be_left
            && matched.is_none_or(|id| {
                let special = self.special(id);
                special.kind != SpecialKind::Left || !special.lhs_strict
            })
        {
            return Ok(None);
        }
        Ok(Some((matched, reversed)))
    }

    /// `create_unique_path` of relation `relids`, the inner relation of
    /// semi join `special`: its cheapest path made unique by the join's
    /// compared columns, already unique when a unique index covers them,
    /// else sorted or hashed as is cheaper; `None` when it cannot be.
    fn unique_path(&mut self, relids: Relids, special: u8) -> Result<Option<u32>, SqlError> {
        let mut rel = self.rel(relids);
        if let Some(held) = rel.unique {
            return Ok(held);
        }
        let info = *self.special(special);
        let mut result = None;
        if info.semi_can_btree || info.semi_can_hash {
            // The compared columns, each class once (`pathkey_is_redundant`).
            let mut keys = [Member::plain(0, 0); MAX_KEYS];
            let mut classes = [u8::MAX; MAX_KEYS];
            let mut count = 0;
            for &member in &info.semi_rhs[..usize::from(info.semi_count)] {
                let class = self
                    .problem
                    .classes
                    .iter()
                    .position(|class| class.members().contains(&member));
                if let Some(class) = class {
                    if self.problem.classes[class].has_const
                        || classes[..count].contains(&(class as u8))
                    {
                        continue;
                    }
                    classes[count] = class as u8;
                }
                keys[count] = member;
                count += 1;
            }
            if count > 0 {
                result = self.build_unique_path(&rel, &keys[..count], &info)?;
            }
        }
        rel.unique = Some(result);
        self.rels[(relids & BASE_RELIDS) as usize] = Some(rel);
        Ok(result)
    }

    fn build_unique_path(
        &mut self,
        rel: &Rel,
        keys: &[Member],
        info: &SpecialJoin,
    ) -> Result<Option<u32>, SqlError> {
        let settings = self.problem.settings;
        let input = self.path(rel.cheapest_total);
        let mut held = [Member::plain(0, 0); MAX_KEYS];
        held[..keys.len()].copy_from_slice(keys);
        let unique = |method: UniqueMethod| PathKind::Unique {
            input: rel.cheapest_total,
            keys: held,
            key_count: keys.len() as u8,
            method,
        };
        // A unique index of a base relation over the columns, with its
        // restrictions' constant equalities, makes it unique already.
        if info.semi_can_btree && rel.relids.count_ones() == 1 {
            let base = &self.problem.rels[rel.relids.trailing_zeros() as usize];
            let mut matched = base.fixed;
            for key in keys {
                if key.column < 64 {
                    matched |= 1 << key.column;
                }
            }
            if base.unique.iter().any(|&index| index & !matched == 0) {
                return self
                    .push_path(Path {
                        kind: unique(UniqueMethod::Noop),
                        rows: rel.rows,
                        ..input
                    })
                    .map(Some);
            }
        }
        let rows = self.statistics.groups(keys, rel.rows)?;
        let columns = keys.len() as f64;
        let costs = settings.costs;
        let sorted = info.semi_can_btree.then(|| {
            let (startup, run) = super::paths::tuplesort_cost(rel.rows, input.width, None, costs);
            let startup = startup + input.total;
            (
                startup,
                startup + run + costs.cpu_operator * rel.rows * columns,
                input.disabled + u32::from(!settings.sort),
            )
        });
        let hashed = (info.semi_can_hash && (input.width + 64.0) * rows <= settings.hash_mem_bytes)
            .then(|| {
                let (startup, total, _) = super::aggregate::cost_agg(
                    super::aggregate::AggStrategy::Hashed,
                    &super::aggregate::AggregateCosts::default(),
                    keys.len(),
                    rows,
                    super::aggregate::AggInput {
                        startup: input.startup,
                        total: input.total,
                        rows: rel.rows,
                        width: input.width,
                    },
                );
                (startup, total, input.disabled)
            });
        let (startup, total, disabled, hash) = match (sorted, hashed) {
            (Some(sort), Some(hash)) => {
                if hash.2 < sort.2 || (hash.2 == sort.2 && hash.1 < sort.1) {
                    (hash.0, hash.1, hash.2, true)
                } else {
                    (sort.0, sort.1, sort.2, false)
                }
            }
            (Some(sort), None) => (sort.0, sort.1, sort.2, false),
            (None, Some(hash)) => (hash.0, hash.1, hash.2, true),
            (None, None) => return Ok(None),
        };
        self.push_path(Path {
            kind: unique(if hash {
                UniqueMethod::Hash
            } else {
                UniqueMethod::Sort
            }),
            rows,
            startup,
            total,
            disabled,
            keys: Keys::NONE,
            ..input
        })
        .map(Some)
    }

    /// The unique-ified path of a semi join's inner relation, which
    /// `join_is_legal` already built.
    fn unique_of(&mut self, relids: Relids, special: Option<u8>) -> Result<u32, SqlError> {
        let special = special.expect("a semi join");
        Ok(self
            .unique_path(relids, special)?
            .expect("a unique-ified relation"))
    }

    /// `add_outer_joins_to_relids`.
    fn add_outer_joins(&self, input: Relids, special: Option<u8>) -> Relids {
        let Some(special) = special.map(|id| self.special(id)) else {
            return input;
        };
        if special.oj == 0 {
            return input;
        }
        if special.kind != SpecialKind::Left {
            return input | special.oj;
        }
        if special.commute_below_l & !input != 0 {
            return input;
        }
        let mut relids = input | special.oj;
        if special.commute_above_l != 0 {
            let mut above = special.commute_above_l;
            for other in self.specials {
                if core::ptr::eq(other, special) || other.oj == 0 || other.kind != SpecialKind::Left
                {
                    continue;
                }
                if other.oj & above == 0 {
                    continue;
                }
                if other.oj & relids == 0
                    && other.min_left & !relids == 0
                    && other.min_right & !relids == 0
                    && other.commute_below_l & !relids == 0
                {
                    relids |= other.oj;
                    above |= other.commute_above_l;
                }
            }
        }
        relids
    }

    /// `join_search_one_level`.
    fn one_level(&mut self, level: usize) -> Result<(), SqlError> {
        let previous = self.levels[level - 1].as_slice();
        let singles = self.levels[1].as_slice();
        for (position, &old) in previous.iter().enumerate() {
            if self.has_joins(old) {
                let first = if level == 2 { position + 1 } else { 0 };
                for &other in &singles[first.min(singles.len())..] {
                    if old & other == 0
                        && (self.relevant(old, other) || self.order_restriction(old, other)?)
                    {
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
                    if old & new == 0
                        && (self.relevant(old, new) || self.order_restriction(old, new)?)
                    {
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
        }
        Ok(())
    }

    /// `make_join_rel`: builds the relation on its first pair, sized by
    /// that pair's clauses, and adds the paths of the pair's join
    /// (`populate_joinrel_with_paths`).
    fn make_join_rel(&mut self, rel1: Relids, rel2: Relids, level: usize) -> Result<(), SqlError> {
        let Some((special, reversed)) = self.join_is_legal(rel1, rel2, rel1 | rel2)? else {
            return Ok(());
        };
        let joined = self.add_outer_joins(rel1 | rel2, special);
        let (rel1, rel2) = if reversed { (rel2, rel1) } else { (rel1, rel2) };
        let restrict = self.restrictlist(rel1, rel2, joined)?;
        let index = (joined & BASE_RELIDS) as usize;
        match self.rels[index] {
            None => {
                let (outer, inner) = (self.rel(rel1), self.rel(rel2));
                let rows = self.join_size(
                    joined,
                    (rel1, rel2),
                    outer.rows,
                    inner.rows,
                    special,
                    &restrict,
                )?;
                let joininfo = self.joinrel_joininfo(joined, rel1, rel2)?;
                self.rels[index] = Some(Rel {
                    relids: joined,
                    joininfo,
                    rows,
                    width: self.width(joined),
                    paths: [0; MAX_REL_PATHS],
                    count: 0,
                    cheapest_total: 0,
                    cheapest_startup: 0,
                    parameterized: [0; MAX_REL_PATHS],
                    parameterized_count: 0,
                    first: (rel1, rel2),
                    unique: None,
                });
                self.levels[level].push(joined).map_err(|_| arena_full())?;
            }
            Some(rel) if rel.relids != joined => {
                self.outside = Some("a join relation formed with different outer joins");
                return Ok(());
            }
            Some(_) => {}
        }
        if self.is_dummy(joined) {
            return Ok(());
        }
        let kind = special.map(|id| self.special(id).kind);
        // `populate_joinrel_with_paths`' proofs of an empty join: a dummy
        // input or the constant false condition of a class of two
        // constants, which is pushed down wherever it is evaluated.
        let false_relids = self.problem.false_relids;
        let constant_false = false_relids.count_ones() > 1
            && false_relids & !(joined & BASE_RELIDS) == 0
            && false_relids & !rel1 != 0
            && false_relids & !rel2 != 0;
        let (dummy1, dummy2) = (self.is_dummy(rel1), self.is_dummy(rel2));
        let empty = match kind {
            None | Some(SpecialKind::Semi) => dummy1 || dummy2 || constant_false,
            Some(SpecialKind::Left | SpecialKind::Anti) => dummy1 || constant_false,
            Some(SpecialKind::Full) => (dummy1 && dummy2) || constant_false,
        };
        if empty {
            return self.mark_dummy(joined);
        }
        let pairs: [(Relids, Relids, JoinType); 2] = match kind {
            None => [(rel1, rel2, JoinType::Inner), (rel2, rel1, JoinType::Inner)],
            Some(SpecialKind::Left) => {
                [(rel1, rel2, JoinType::Left), (rel2, rel1, JoinType::Right)]
            }
            Some(SpecialKind::Full) => [(rel1, rel2, JoinType::Full), (rel2, rel1, JoinType::Full)],
            Some(SpecialKind::Anti) => [
                (rel1, rel2, JoinType::Anti),
                (rel2, rel1, JoinType::RightAnti),
            ],
            Some(SpecialKind::Semi) => {
                let id = special.expect("a semi join");
                let info = *self.special(id);
                if info.min_left & !rel1 == 0 && info.min_right & !rel2 == 0 {
                    self.add_paths(joined, rel1, rel2, JoinType::Semi, special, restrict)?;
                    self.add_paths(joined, rel2, rel1, JoinType::RightSemi, special, restrict)?;
                }
                if info.syn_right == rel2 && self.unique_path(rel2, id)?.is_some() {
                    self.add_paths(joined, rel1, rel2, JoinType::UniqueInner, special, restrict)?;
                    self.add_paths(joined, rel2, rel1, JoinType::UniqueOuter, special, restrict)?;
                }
                return Ok(());
            }
        };
        for (outer, inner, jointype) in pairs {
            self.add_paths(joined, outer, inner, jointype, special, restrict)?;
        }
        if kind == Some(SpecialKind::Full) && self.rel(joined).count == 0 {
            return Err(crate::sql_err!(
                crate::sql::eval::sqlstate::FEATURE_NOT_SUPPORTED,
                "FULL JOIN is only supported with merge-joinable or hash-joinable join conditions"
            ));
        }
        Ok(())
    }

    /// `is_dummy_rel`.
    fn is_dummy(&self, relids: Relids) -> bool {
        let rel = self.rel(relids);
        rel.count > 0 && matches!(self.path(rel.cheapest_total).kind, PathKind::Dummy)
    }

    /// `mark_dummy_rel`: the relation returns no rows, its one path a
    /// `Result` whose condition is constant false.
    fn mark_dummy(&mut self, relids: Relids) -> Result<(), SqlError> {
        let mut rel = self.rel(relids);
        let id = self.push_path(Path {
            kind: PathKind::Dummy,
            relids: rel.relids,
            rows: 0.0,
            width: rel.width,
            startup: 0.0,
            total: 0.0,
            disabled: 0,
            keys: Keys::NONE,
            param: 0,
        })?;
        rel.rows = 0.0;
        rel.paths[0] = id;
        rel.count = 1;
        rel.cheapest_total = id;
        rel.cheapest_startup = id;
        rel.parameterized[0] = id;
        rel.parameterized_count = 1;
        self.rels[(relids & BASE_RELIDS) as usize] = Some(rel);
        Ok(())
    }

    /// `build_joinrel_restrictlist`: the clauses of the inputs' `joininfo`
    /// the pair can evaluate (the outer relation's, then the inner's),
    /// then one equality per equivalence class spanning them
    /// (`generate_join_implied_equalities`).
    fn restrictlist(
        &mut self,
        outer: Relids,
        inner: Relids,
        joined: Relids,
    ) -> Result<ClauseList, SqlError> {
        let both = outer | inner;
        let mut list = ClauseList::EMPTY;
        let lists = [self.rel(outer).joininfo, self.rel(inner).joininfo];
        for joininfo in lists {
            for &id in joininfo.as_slice() {
                let clause = self.clause(id);
                if clause.required & !joined != 0 {
                    continue;
                }
                // A copy of a commuting outer join's clause for another
                // order of the joins, which this pair's inputs must not
                // already have performed.
                if clause.clone != CloneKind::None
                    && (clause.required & !both != 0 || clause.incompatible & both != 0)
                {
                    continue;
                }
                if !list.contains(id) {
                    list.push(id).ok_or_else(too_many)?;
                }
            }
        }
        for (class_id, class) in self.problem.classes.iter().enumerate() {
            if class.has_const || class.count <= 1 {
                continue;
            }
            let members = class.members();
            let computable = |member: &&Member| member.relids() & !joined == 0;
            let outer_member = members
                .iter()
                .filter(computable)
                .position(|member| member.relids() & !outer == 0);
            let inner_member = members
                .iter()
                .filter(computable)
                .position(|member| member.relids() & !inner == 0);
            let (Some(_), Some(_)) = (outer_member, inner_member) else {
                continue;
            };
            let outer_member = members
                .iter()
                .position(|member| member.relids() & !joined == 0 && member.relids() & !outer == 0)
                .expect("found");
            let inner_member = members
                .iter()
                .position(|member| member.relids() & !joined == 0 && member.relids() & !inner == 0)
                .expect("found");
            let id = self.class_clause(class_id as u8, outer_member as u8, inner_member as u8)?;
            list.push(id).ok_or_else(too_many)?;
        }
        Ok(list)
    }

    /// `build_joinrel_joinlist`: the inputs' clauses the join cannot yet
    /// evaluate.
    fn joinrel_joininfo(
        &self,
        joined: Relids,
        outer: Relids,
        inner: Relids,
    ) -> Result<ClauseList, SqlError> {
        let mut list = ClauseList::EMPTY;
        for joininfo in [self.rel(outer).joininfo, self.rel(inner).joininfo] {
            for &id in joininfo.as_slice() {
                if self.clause(id).required & !joined != 0 && !list.contains(id) {
                    list.push(id).ok_or_else(too_many)?;
                }
            }
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
        let relids = members[usize::from(outer)].relids() | members[usize::from(inner)].relids();
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
                required: relids,
                pushed_down: true,
                clone: CloneKind::None,
                incompatible: 0,
                cost,
                selectivity: None,
                outer_selectivity: None,
                outer_relids: 0,
                operands: None,
                other: OtherShape::Plain,
                source: None,
            })
            .map_err(|_| arena_full())?;
        Ok(id as u8)
    }

    /// `clause_selectivity` of a join clause, cached on the clause as
    /// PostgreSQL caches it on its `RestrictInfo`: one estimate for inner
    /// joins and one, whichever join computed it first, for the others.
    fn selectivity(&mut self, id: u8, mode: Selectivity) -> Result<f64, SqlError> {
        let clause = self.clause(id);
        let cached = match mode {
            Selectivity::Inner => clause.selectivity,
            Selectivity::Outer(_) => clause.outer_selectivity,
        };
        if let Some(selectivity) = cached {
            return Ok(selectivity);
        }
        let semi = match mode {
            Selectivity::Outer(Some(special)) => matches!(
                self.special(special).kind,
                SpecialKind::Semi | SpecialKind::Anti
            ),
            _ => false,
        };
        let selectivity = match self.sides(id) {
            Some((left, right, ..)) if semi => {
                // `eqjoinsel` of a semi or anti join: its inner side is the
                // member of the join's syntactic inner relations, and its
                // inner input is the relation of its minimal inner
                // relations (`find_join_input_rel`), no more than the inner
                // join's estimate allows.
                let Selectivity::Outer(Some(special)) = mode else {
                    unreachable!("a semi join's estimate");
                };
                let special = *self.special(special);
                let (outer, inner) = if left.relids() & !special.syn_right & BASE_RELIDS == 0 {
                    (right, left)
                } else {
                    (left, right)
                };
                let Some(inner_rel) = self.rels[(special.min_right & BASE_RELIDS) as usize] else {
                    self.outside = Some("a semi join whose inner relation is not yet built");
                    return Ok(1.0);
                };
                let inner_selectivity = self.statistics.equality(left, right)?.clamp(0.0, 1.0);
                self.statistics
                    .semi(outer, inner, inner_rel.rows)?
                    .min(inner_rel.rows * inner_selectivity)
                    .clamp(0.0, 1.0)
            }
            Some((left, right, ..)) => self.statistics.equality(left, right)?.clamp(0.0, 1.0),
            None if semi && clause.other != OtherShape::Plain => {
                let Selectivity::Outer(Some(special)) = mode else {
                    unreachable!("a semi join's estimate");
                };
                let syn_right = self.special(special).syn_right;
                match (clause.other, clause.operands) {
                    (
                        OtherShape::Inequality {
                            left_nulls,
                            right_nulls,
                        },
                        Some((left, _)),
                    ) => {
                        if left.relids() & syn_right & BASE_RELIDS == 0 {
                            1.0 - left_nulls
                        } else {
                            1.0 - right_nulls
                        }
                    }
                    _ => {
                        self.outside = Some("an OR condition of a semi or anti join");
                        1.0
                    }
                }
            }
            // Other clauses come with their estimate.
            None => clause
                .selectivity
                .or(clause.outer_selectivity)
                .unwrap_or(1.0),
        };
        let held = self.clauses.borrowed_slice()[usize::from(id)];
        let updated = match mode {
            Selectivity::Inner => Clause {
                selectivity: Some(selectivity),
                ..held
            },
            Selectivity::Outer(_) => Clause {
                outer_selectivity: Some(selectivity),
                ..held
            },
        };
        self.set_clause(id, updated)?;
        Ok(selectivity)
    }

    /// `clauselist_selectivity` of join clauses: their product.
    fn clauselist_selectivity(
        &mut self,
        clauses: &ClauseList,
        mode: Selectivity,
    ) -> Result<f64, SqlError> {
        let mut selectivity = 1.0;
        for &id in clauses.as_slice() {
            selectivity *= self.selectivity(id, mode)?;
        }
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
    /// its members that a relation or outer join outside it, or the
    /// output, reads.
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
        self.rels[(relids & BASE_RELIDS) as usize] = Some(rel);
    }

    /// `add_path` without parallel paths into `relids`' relation.
    fn add_path(&mut self, relids: Relids, new: Path) -> Result<(), SqlError> {
        let mut rel = self.rel(relids);
        let id = self.paths.len() as u32;
        let paths = self.paths.as_slice();
        let (count, accepted) = add_to_pathlist(
            &mut rel.paths,
            usize::from(rel.count),
            (id, new),
            &|id| paths[id as usize],
            self.consider_startup,
        )?;
        if accepted {
            self.push_path(new)?;
        }
        rel.count = count as u8;
        self.rels[(relids & BASE_RELIDS) as usize] = Some(rel);
        Ok(())
    }

    /// `create_ordered_paths` over the joined relation `rel`'s paths, then
    /// `create_limit_path` over each, and the cheapest of those
    /// (`get_cheapest_fractional_path` of a tuple fraction of zero): the
    /// join's path under the nodes above it, and those nodes.
    fn upper_paths(&mut self, rel: &Rel) -> Result<(u32, UpperPlan), SqlError> {
        const MAX_UPPER: usize = 2 * MAX_REL_PATHS;
        let upper = self.problem.upper;
        if let Some(group) = upper.group {
            return self.grouped_paths(rel, group);
        }
        let settings = self.problem.settings;
        let sort = upper.sort;
        let sort_len = usize::from(sort.len);
        let limit_tuples = upper
            .limit
            .and_then(|(count, offset)| count.map(|count| count.max(1.0) + offset.max(0.0)));
        let cheapest = rel.cheapest_total;
        // Each upper path, the join path below it and its sort and limit.
        let mut store = [self.path(cheapest); MAX_UPPER];
        let mut below = [(cheapest, None::<(f64, f64, u8)>); MAX_UPPER];
        let mut stored = 0usize;
        let mut ordered = [0u32; MAX_REL_PATHS];
        let mut ordered_count = 0usize;
        for &id in &rel.paths[..usize::from(rel.count)] {
            let mut input = self.path(id);
            input.total += upper.target_cost * input.rows;
            let mut path = input;
            let mut sorted = None;
            let presorted = sort.common_prefix(&input.keys);
            if sort_len > 0 && presorted < sort_len {
                let incremental = presorted > 0 && settings.incremental_sort;
                if id != cheapest && !incremental {
                    continue;
                }
                let costs = settings.costs;
                let (startup, total) = if incremental {
                    self.incremental_sort_cost(&input, &sort, presorted, upper.width, limit_tuples)?
                } else {
                    let (startup, run) =
                        super::paths::tuplesort_cost(input.rows, upper.width, limit_tuples, costs);
                    let startup = startup + input.total;
                    path.disabled += u32::from(!settings.sort);
                    (startup, startup + run)
                };
                path.startup = startup;
                path.total = total;
                path.keys = sort;
                sorted = Some((
                    startup,
                    total,
                    if incremental { presorted as u8 } else { 0 },
                ));
            }
            if sort_len == 0 {
                // Without an ordering the joined relation's paths are the
                // final relation's.
                ordered[ordered_count] = stored as u32;
                ordered_count += 1;
                store[stored] = path;
                below[stored] = (id, None);
                stored += 1;
                continue;
            }
            store[stored] = path;
            below[stored] = (id, sorted);
            let (count, accepted) = add_to_pathlist(
                &mut ordered,
                ordered_count,
                (stored as u32, path),
                &|at| store[at as usize],
                self.consider_startup,
            )?;
            ordered_count = count;
            stored += usize::from(accepted);
        }
        // `create_limit_path` scales each path's run cost to the rows it
        // reads.
        let mut finals = [0u32; MAX_REL_PATHS];
        let mut final_count = 0usize;
        let mut limits = [None::<(f64, f64, f64)>; MAX_UPPER];
        for &at in &ordered[..ordered_count] {
            let mut path = store[at as usize];
            let mut limit = None;
            if let Some((count, offset)) = upper.limit {
                let (startup, total, rows) = super::paths::adjust_limit_rows_costs(
                    path.startup,
                    path.total,
                    path.rows,
                    count,
                    offset,
                );
                path.startup = startup;
                path.total = total;
                path.rows = rows;
                limit = Some((startup, total, rows));
            }
            if stored == MAX_UPPER {
                return Err(too_many());
            }
            store[stored] = path;
            below[stored] = below[at as usize];
            limits[stored] = limit;
            let (count, accepted) = add_to_pathlist(
                &mut finals,
                final_count,
                (stored as u32, path),
                &|at| store[at as usize],
                self.consider_startup,
            )?;
            final_count = count;
            stored += usize::from(accepted);
        }
        // `set_cheapest`'s cheapest total path.
        let mut chosen = finals[0];
        for &at in &finals[1..final_count] {
            let compared = compare_costs(&store[chosen as usize], &store[at as usize], false);
            if compared > 0
                || (compared == 0
                    && compare_keys(&store[chosen as usize].keys, &store[at as usize].keys)
                        == KeysOrder::Better2)
            {
                chosen = at;
            }
        }
        let (top, sort) = below[chosen as usize];
        let joined = self.path(top);
        Ok((
            top,
            UpperPlan {
                sort,
                limit: limits[chosen as usize],
                top_total: joined.total + upper.target_cost * joined.rows,
                group: None,
            },
        ))
    }

    /// `cost_incremental_sort` of `input`'s rows of `width`, sorted by
    /// `keys` of which the first `presorted` already are: its startup and
    /// total cost.
    fn incremental_sort_cost(
        &self,
        input: &Path,
        keys: &Keys,
        presorted: usize,
        width: f64,
        limit_tuples: Option<f64>,
    ) -> Result<(f64, f64), SqlError> {
        let costs = self.problem.settings.costs;
        let input_tuples = input.rows.max(2.0);
        let mut members = [Member::plain(0, 0); MAX_KEYS];
        for (at, &key) in keys.as_slice()[..presorted].iter().enumerate() {
            members[at] = self.problem.classes[usize::from(key & KEY_CLASS)].members[0];
        }
        let groups = self
            .statistics
            .groups(&members[..presorted], input_tuples)?;
        let group_tuples = input_tuples / groups;
        let input_run = input.total - input.startup;
        let group_input_run = input_run / groups;
        let (group_startup, group_run) =
            super::paths::tuplesort_cost(group_tuples, width, limit_tuples, costs);
        let startup = group_startup + input.startup + group_input_run;
        let mut run = group_run
            + (group_run + group_startup) * (groups - 1.0)
            + group_input_run * (groups - 1.0);
        // `create_incremental_sort_path` passes no comparison cost.
        run += costs.cpu_tuple * input_tuples;
        run += 2.0 * costs.cpu_tuple * groups;
        Ok((startup, startup + run))
    }

    /// `add_paths_to_grouping_rel` of a `GROUP BY` without `HAVING`: a
    /// `GroupAggregate` over each joined path sorted by the grouping
    /// pathkeys or by an order of them its pathkeys begin
    /// (`get_useful_group_keys_orderings`, `make_ordered_path`), and a
    /// `HashAggregate` over the cheapest; then `create_ordered_paths` and
    /// `create_limit_path` over the grouped paths.
    fn grouped_paths(
        &mut self,
        rel: &Rel,
        group: GroupProblem,
    ) -> Result<(u32, UpperPlan), SqlError> {
        use super::aggregate::{AggInput, AggStrategy, cost_agg};
        const MAX_GROUPED: usize = 3 * MAX_REL_PATHS;
        let upper = self.problem.upper;
        let settings = self.problem.settings;
        let costs = settings.costs;
        let columns = group.keys.as_slice().len();
        let plain = columns == 0;
        let cheapest = rel.cheapest_total;
        // `get_number_of_groups` over the joined relation's rows; plain
        // aggregation returns one.
        let groups = if plain {
            1.0
        } else {
            self.statistics
                .num_groups(&group.members[..columns], self.path(cheapest).rows)?
        };
        // `create_agg_path`: `cost_agg` with the `HAVING` quals, then the
        // target list's cost per group it returns.
        let aggregate = |strategy: AggStrategy, input: AggInput| {
            let (startup, total, rows) = super::aggregate::with_having(
                cost_agg(strategy, &group.aggregates, columns, groups, input),
                group.having,
            );
            (startup, total + group.target_cost * rows, rows)
        };
        // Each grouped path, the join path below it and its plan.
        let mut store = [self.path(cheapest); MAX_GROUPED];
        let mut plans = [(cheapest, None::<GroupPlan>); MAX_GROUPED];
        let mut stored = 0usize;
        let mut grouped = [0u32; MAX_REL_PATHS];
        let mut grouped_count = 0usize;
        for &id in &rel.paths[..usize::from(rel.count)] {
            let mut input = self.path(id);
            if input.param != 0 {
                continue;
            }
            input.total += upper.target_cost * input.rows;
            let mut orderings = [group.keys; 2];
            let mut ordering_count = 1;
            if group.group_by_reordering
                && input.keys.len > 0
                && !input.keys.contained_in(&group.keys)
            {
                // `group_keys_reorder_by_pathkeys`: the path's leading
                // grouping keys first, then the others in their order.
                let mut reordered = Keys::NONE;
                for &key in input.keys.as_slice() {
                    if reordered.as_slice().len() >= columns
                        || !group.keys.as_slice().contains(&key)
                    {
                        break;
                    }
                    let _ = reordered.push(key);
                }
                let matched = reordered.as_slice().len();
                for &key in group.keys.as_slice() {
                    if !reordered.as_slice().contains(&key) {
                        let _ = reordered.push(key);
                    }
                }
                if matched > 0
                    && (settings.incremental_sort || matched == columns)
                    && reordered != group.keys
                {
                    orderings[1] = reordered;
                    ordering_count = 2;
                }
            }
            for ordering in &orderings[..ordering_count] {
                let presorted = ordering.common_prefix(&input.keys);
                let mut sorted = input;
                let mut input_sort = None;
                if presorted < columns {
                    let incremental = presorted > 0 && settings.incremental_sort;
                    if id != cheapest && !incremental {
                        continue;
                    }
                    let (startup, total) = if incremental {
                        self.incremental_sort_cost(
                            &input,
                            ordering,
                            presorted,
                            group.input_width,
                            None,
                        )?
                    } else {
                        let (startup, run) = super::paths::tuplesort_cost(
                            input.rows,
                            group.input_width,
                            None,
                            costs,
                        );
                        sorted.disabled += u32::from(!settings.sort);
                        (startup + input.total, startup + input.total + run)
                    };
                    sorted.startup = startup;
                    sorted.total = total;
                    sorted.keys = *ordering;
                    input_sort = Some((
                        startup,
                        total,
                        if incremental { presorted as u8 } else { 0 },
                    ));
                }
                let (startup, total, rows) = if group.distinct {
                    // `create_upper_unique_path`.
                    (
                        sorted.startup,
                        sorted.total + costs.cpu_operator * input.rows * columns as f64,
                        groups,
                    )
                } else {
                    aggregate(
                        if plain {
                            AggStrategy::Plain
                        } else {
                            AggStrategy::Sorted
                        },
                        AggInput {
                            startup: sorted.startup,
                            total: sorted.total,
                            rows: input.rows,
                            width: group.input_width,
                        },
                    )
                };
                if stored == MAX_GROUPED {
                    return Err(too_many());
                }
                let path = Path {
                    startup,
                    total,
                    rows,
                    width: group.output_width,
                    keys: if plain { Keys::NONE } else { sorted.keys },
                    ..sorted
                };
                store[stored] = path;
                plans[stored] = (
                    id,
                    Some(GroupPlan {
                        hashed: false,
                        keys: *ordering,
                        input_sort,
                        startup,
                        total,
                        rows,
                        width: group.output_width,
                        groups,
                        input: AggregateInput { rows: input.rows },
                    }),
                );
                let (count, accepted) = add_to_pathlist(
                    &mut grouped,
                    grouped_count,
                    (stored as u32, path),
                    &|at| store[at as usize],
                    self.consider_startup,
                )?;
                grouped_count = count;
                stored += usize::from(accepted);
            }
        }
        // A `DISTINCT` hashes only with `enable_hashagg` on, unless it has
        // no other path.
        if group.hashable && !plain && (!group.distinct || group.hashagg || grouped_count == 0) {
            let mut input = self.path(cheapest);
            input.total += upper.target_cost * input.rows;
            let (startup, total, rows) = aggregate(
                AggStrategy::Hashed,
                AggInput {
                    startup: input.startup,
                    total: input.total,
                    rows: input.rows,
                    width: group.input_width,
                },
            );
            if stored == MAX_GROUPED {
                return Err(too_many());
            }
            let path = Path {
                startup,
                total,
                rows,
                width: group.output_width,
                keys: Keys::NONE,
                disabled: input.disabled + u32::from(!group.hashagg),
                ..input
            };
            store[stored] = path;
            plans[stored] = (
                cheapest,
                Some(GroupPlan {
                    hashed: true,
                    keys: group.keys,
                    input_sort: None,
                    startup,
                    total,
                    rows,
                    width: group.output_width,
                    groups,
                    input: AggregateInput { rows: input.rows },
                }),
            );
            let (count, accepted) = add_to_pathlist(
                &mut grouped,
                grouped_count,
                (stored as u32, path),
                &|at| store[at as usize],
                self.consider_startup,
            )?;
            grouped_count = count;
            stored += usize::from(accepted);
        }
        // `set_cheapest` of the grouped relation.
        let mut cheapest_grouped = grouped[0];
        for &at in &grouped[1..grouped_count] {
            let compared = compare_costs(
                &store[cheapest_grouped as usize],
                &store[at as usize],
                false,
            );
            if compared > 0
                || (compared == 0
                    && compare_keys(
                        &store[cheapest_grouped as usize].keys,
                        &store[at as usize].keys,
                    ) == KeysOrder::Better2)
            {
                cheapest_grouped = at;
            }
        }
        // `create_ordered_paths` over the grouped paths, then
        // `create_limit_path`.
        let sort = group.sort;
        let sort_len = usize::from(sort.len);
        let limit_tuples = upper
            .limit
            .and_then(|(count, offset)| count.map(|count| count.max(1.0) + offset.max(0.0)));
        let mut sorts = [None::<(f64, f64, u8)>; MAX_GROUPED];
        let mut ordered = [0u32; MAX_REL_PATHS];
        let mut ordered_count = 0usize;
        for &at in &grouped[..grouped_count] {
            let input = store[at as usize];
            let mut path = input;
            let mut sorted = None;
            let presorted = sort.common_prefix(&input.keys);
            if sort_len > 0 && presorted < sort_len {
                let incremental = presorted > 0 && settings.incremental_sort;
                if at != cheapest_grouped && !incremental {
                    continue;
                }
                let (startup, total) = if incremental {
                    self.incremental_sort_cost(
                        &input,
                        &sort,
                        presorted,
                        group.output_width,
                        limit_tuples,
                    )?
                } else {
                    let (startup, run) = super::paths::tuplesort_cost(
                        input.rows,
                        group.output_width,
                        limit_tuples,
                        costs,
                    );
                    path.disabled += u32::from(!settings.sort);
                    (startup + input.total, startup + input.total + run)
                };
                path.startup = startup;
                path.total = total;
                path.keys = sort;
                sorted = Some((
                    startup,
                    total,
                    if incremental { presorted as u8 } else { 0 },
                ));
            }
            if stored == MAX_GROUPED {
                return Err(too_many());
            }
            store[stored] = path;
            plans[stored] = plans[at as usize];
            sorts[stored] = sorted;
            if sort_len == 0 {
                ordered[ordered_count] = stored as u32;
                ordered_count += 1;
                stored += 1;
                continue;
            }
            let (count, accepted) = add_to_pathlist(
                &mut ordered,
                ordered_count,
                (stored as u32, path),
                &|at| store[at as usize],
                self.consider_startup,
            )?;
            ordered_count = count;
            stored += usize::from(accepted);
        }
        let mut finals = [0u32; MAX_REL_PATHS];
        let mut final_count = 0usize;
        let mut limits = [None::<(f64, f64, f64)>; MAX_GROUPED];
        for &at in &ordered[..ordered_count] {
            let mut path = store[at as usize];
            let mut limit = None;
            if let Some((count, offset)) = upper.limit {
                let (startup, total, rows) = super::paths::adjust_limit_rows_costs(
                    path.startup,
                    path.total,
                    path.rows,
                    count,
                    offset,
                );
                path.startup = startup;
                path.total = total;
                path.rows = rows;
                limit = Some((startup, total, rows));
            }
            if stored == MAX_GROUPED {
                return Err(too_many());
            }
            store[stored] = path;
            plans[stored] = plans[at as usize];
            sorts[stored] = sorts[at as usize];
            limits[stored] = limit;
            let (count, accepted) = add_to_pathlist(
                &mut finals,
                final_count,
                (stored as u32, path),
                &|at| store[at as usize],
                self.consider_startup,
            )?;
            final_count = count;
            stored += usize::from(accepted);
        }
        let mut chosen = finals[0];
        for &at in &finals[1..final_count] {
            let compared = compare_costs(&store[chosen as usize], &store[at as usize], false);
            if compared > 0
                || (compared == 0
                    && compare_keys(&store[chosen as usize].keys, &store[at as usize].keys)
                        == KeysOrder::Better2)
            {
                chosen = at;
            }
        }
        let (top, plan) = plans[chosen as usize];
        let joined = self.path(top);
        Ok((
            top,
            UpperPlan {
                sort: sorts[chosen as usize],
                limit: limits[chosen as usize],
                top_total: joined.total + upper.target_cost * joined.rows,
                group: plan,
            },
        ))
    }

    /// `eclass_useful_for_merging`.
    fn useful_for_merging(&self, class: u8, relids: Relids) -> bool {
        let class = &self.problem.classes[usize::from(class)];
        if class.has_const || class.count <= 1 || class.relids() & !relids == 0 {
            return false;
        }
        class
            .members()
            .iter()
            .any(|member| member.relids() & relids == 0)
    }

    /// `build_join_pathkeys`: an outer join that may return the inner
    /// relation's rows alone keeps no order; otherwise the outer keys
    /// still useful for merging above it (`truncate_useless_pathkeys`
    /// with `pathkeys_useful_for_merging`).
    fn join_keys(&self, joined: Relids, jointype: JoinType, outer: &Keys) -> Keys {
        if matches!(
            jointype,
            JoinType::Full | JoinType::Right | JoinType::RightAnti
        ) {
            return Keys::NONE;
        }
        let rel = self.rel(joined);
        let eclass_joins = self.has_eclass_joins(joined);
        let mut useful = 0;
        for &key in outer.as_slice() {
            if !self.right_merge_direction(key) {
                break;
            }
            let class = key & KEY_CLASS;
            let matched = if eclass_joins && self.useful_for_merging(class, joined) {
                true
            } else {
                rel.joininfo.as_slice().iter().any(|&id| {
                    self.sides(id)
                        .is_some_and(|(_, _, left, right)| left == class || right == class)
                })
            };
            if matched {
                useful += 1;
            } else {
                break;
            }
        }
        // `pathkeys_useful_for_ordering`.
        let useful = useful.max(self.problem.upper.sort.common_prefix(outer));
        outer.truncated(useful)
    }

    /// `right_merge_direction`: a pathkey is useful for merging in the
    /// direction the query orders its class, ascending when it does not.
    fn right_merge_direction(&self, key: u8) -> bool {
        let wanted = self
            .problem
            .upper
            .sort
            .as_slice()
            .iter()
            .find(|&&query| query & KEY_CLASS == key & KEY_CLASS)
            .map_or(0, |&query| query & KEY_DESCENDING);
        key & KEY_DESCENDING == wanted
    }

    /// `select_mergejoin_clauses`: the join clauses a merge join can use,
    /// and whether the join type may merge at all.
    fn select_mergejoin_clauses(
        &self,
        joined: Relids,
        outer: Relids,
        inner: Relids,
        restrict: &ClauseList,
        jointype: JoinType,
    ) -> (ClauseList, bool) {
        if jointype == JoinType::RightSemi {
            return (ClauseList::EMPTY, false);
        }
        let mut list = ClauseList::EMPTY;
        let mut nonmergeable = false;
        for &id in restrict.as_slice() {
            if jointype.is_outer() && self.clause(id).is_pushed_down(joined) {
                continue;
            }
            let Some((_, _, left, right)) = self.sides(id) else {
                nonmergeable = true;
                continue;
            };
            if self.outer_is_left(id, outer, inner).is_none() {
                nonmergeable = true;
                continue;
            }
            // `EC_MUST_BE_REDUNDANT`.
            if self.problem.classes[usize::from(left)].has_const
                || self.problem.classes[usize::from(right)].has_const
            {
                nonmergeable = true;
                continue;
            }
            let _ = list.push(id);
        }
        let allowed = match jointype {
            JoinType::Right | JoinType::RightAnti | JoinType::Full => !nonmergeable,
            _ => true,
        };
        (list, allowed)
    }

    /// `add_paths_to_joinrel`.
    fn add_paths(
        &mut self,
        joined: Relids,
        outer: Relids,
        inner: Relids,
        jointype: JoinType,
        special: Option<u8>,
        restrict: ClauseList,
    ) -> Result<(), SqlError> {
        let settings = self.problem.settings;
        let inner_unique = match jointype {
            JoinType::Semi | JoinType::Anti => false,
            JoinType::UniqueInner => {
                let special = self.special(special.expect("a semi join"));
                special.min_left & !outer == 0
            }
            JoinType::UniqueOuter => {
                self.inner_unique(joined, outer, inner, JoinType::Inner, &restrict)
            }
            _ => self.inner_unique(joined, outer, inner, jointype, &restrict),
        };
        let (mergeable, merge_allowed) = if settings.mergejoin || jointype == JoinType::Full {
            self.select_mergejoin_clauses(joined, outer, inner, &restrict, jointype)
        } else {
            (ClauseList::EMPTY, true)
        };
        let (match_fraction, match_count) =
            if matches!(jointype, JoinType::Semi | JoinType::Anti) || inner_unique {
                self.semi_factors(joined, inner, jointype, special, &restrict)?
            } else {
                (1.0, 1.0)
            };
        let mut param_source = 0;
        for other in self.specials {
            if joined & other.min_right != 0 && joined & other.min_left == 0 {
                param_source |= self.all_baserels & !other.min_right;
            }
            if other.kind == SpecialKind::Full
                && joined & other.min_left != 0
                && joined & other.min_right == 0
            {
                param_source |= self.all_baserels & !other.min_left;
            }
        }
        let inputs = JoinInputs {
            outer,
            inner,
            jointype,
            special,
            restrict,
            mergeable,
            inner_unique,
            match_fraction,
            match_count,
            param_source,
        };
        if merge_allowed {
            self.sort_inner_and_outer(joined, &inputs)?;
            self.match_unsorted_outer(joined, &inputs)?;
        }
        if settings.hashjoin || jointype == JoinType::Full {
            self.hash_inner_and_outer(joined, &inputs)?;
        }
        Ok(())
    }

    /// `select_outer_pathkeys_for_merge`: the query's pathkeys when the
    /// merge clauses' outer classes hold them all, or their prefix when it
    /// is all the clauses' classes, then the other classes, most future
    /// join partners first.
    fn outer_pathkeys_for_merge(&self, joined: Relids, inputs: &JoinInputs) -> Keys {
        let mut classes = [0u8; MAX_JOIN_CLAUSES];
        let mut scores = [0i32; MAX_JOIN_CLAUSES];
        let mut count = 0;
        for &id in inputs.mergeable.as_slice() {
            let (class, _) = self.merge_classes(id, inputs.outer, inputs.inner);
            if classes[..count].contains(&class) {
                continue;
            }
            scores[count] = self.problem.classes[usize::from(class)]
                .members()
                .iter()
                .filter(|member| member.relids() & joined == 0)
                .count() as i32;
            classes[count] = class;
            count += 1;
        }
        let mut all = Keys::NONE;
        if count == 0 {
            return all;
        }
        let query = self.problem.upper.sort;
        if query.len > 0 {
            let matches = query
                .as_slice()
                .iter()
                .take_while(|&&key| classes[..count].contains(&(key & KEY_CLASS)))
                .count();
            if matches == usize::from(query.len) {
                all = query;
                for &key in query.as_slice() {
                    if let Some(at) = classes[..count]
                        .iter()
                        .position(|&class| class == key & KEY_CLASS)
                    {
                        scores[at] = -1;
                    }
                }
            } else if matches == usize::from(inputs.mergeable.len) {
                return query.truncated(matches);
            }
        }
        loop {
            let mut best = 0;
            for j in 1..count {
                if scores[j] > scores[best] {
                    best = j;
                }
            }
            if scores[best] < 0 {
                break;
            }
            scores[best] = -1;
            let _ = all.push(classes[best]);
        }
        all
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
        let mut outer_path = self.rel(inputs.outer).cheapest_total;
        let mut inner_path = self.rel(inputs.inner).cheapest_total;
        if self.path(outer_path).param & inputs.inner != 0
            || self.path(inner_path).param & inputs.outer != 0
        {
            return Ok(());
        }
        match inputs.jointype {
            JoinType::UniqueOuter => {
                outer_path = self.unique_of(inputs.outer, inputs.special)?;
            }
            JoinType::UniqueInner => {
                inner_path = self.unique_of(inputs.inner, inputs.special)?;
            }
            _ => {}
        }
        let all = self.outer_pathkeys_for_merge(joined, inputs);
        for front in 0..usize::from(all.len) {
            let mut outer_keys = Keys::NONE;
            let _ = outer_keys.push(all.classes[front]);
            for (at, &class) in all.as_slice().iter().enumerate() {
                if at != front {
                    let _ = outer_keys.push(class);
                }
            }
            let merge = self.mergeclauses_for_outer_keys(&outer_keys, inputs);
            let inner_keys = self.inner_keys_for_merge(&merge, inputs, &outer_keys);
            let keys = self.join_keys(joined, inputs.path_jointype(), &outer_keys);
            self.try_mergejoin(
                joined,
                inputs,
                outer_path,
                inner_path,
                keys,
                merge,
                Some(outer_keys),
                Some(inner_keys),
            )?;
        }
        Ok(())
    }

    /// `find_mergeclauses_for_outer_pathkeys`.
    fn mergeclauses_for_outer_keys(&self, keys: &Keys, inputs: &JoinInputs) -> ClauseList {
        let mut merge = ClauseList::EMPTY;
        for &key in keys.as_slice() {
            let mut matched = false;
            for &id in inputs.mergeable.as_slice() {
                if self.merge_classes(id, inputs.outer, inputs.inner).0 == key & KEY_CLASS {
                    let _ = merge.push(id);
                    matched = true;
                }
            }
            if !matched {
                break;
            }
        }
        merge
    }

    /// The direction flags of the outer pathkey of merge clause `id`'s
    /// outer class.
    fn merge_direction(&self, id: u8, inputs: &JoinInputs, outer_keys: &Keys) -> u8 {
        let (class, _) = self.merge_classes(id, inputs.outer, inputs.inner);
        outer_keys
            .as_slice()
            .iter()
            .find(|&&key| key & KEY_CLASS == class)
            .map_or(0, |&key| key & !KEY_CLASS)
    }

    /// `make_inner_pathkeys_for_merge`: each merge clause's inner class,
    /// once, sorted in its outer pathkey's direction.
    fn inner_keys_for_merge(
        &self,
        merge: &ClauseList,
        inputs: &JoinInputs,
        outer_keys: &Keys,
    ) -> Keys {
        let mut keys = Keys::NONE;
        for &id in merge.as_slice() {
            let (_, class) = self.merge_classes(id, inputs.outer, inputs.inner);
            if !self.problem.classes[usize::from(class)].has_const
                && !keys.as_slice().iter().any(|&key| key & KEY_CLASS == class)
            {
                let _ = keys.push(class | self.merge_direction(id, inputs, outer_keys));
            }
        }
        keys
    }

    /// `match_unsorted_outer`.
    fn match_unsorted_outer(
        &mut self,
        joined: Relids,
        inputs: &JoinInputs,
    ) -> Result<(), SqlError> {
        let (nestloop_ok, use_all) = match inputs.jointype {
            JoinType::RightSemi => return Ok(()),
            JoinType::Inner
            | JoinType::Left
            | JoinType::Semi
            | JoinType::Anti
            | JoinType::UniqueInner
            | JoinType::UniqueOuter => (true, false),
            JoinType::Right | JoinType::RightAnti | JoinType::Full => (false, true),
        };
        let outer_rel = self.rel(inputs.outer);
        let inner_rel = self.rel(inputs.inner);
        let settings = self.problem.settings;
        let mut inner_total = Some(inner_rel.cheapest_total)
            .filter(|&path| self.path(path).param & inputs.outer == 0);
        let mut material = None;
        if inputs.jointype == JoinType::UniqueInner {
            if inner_total.is_none() {
                return Ok(());
            }
            inner_total = Some(self.unique_of(inputs.inner, inputs.special)?);
        } else if let Some(total) = inner_total
            && nestloop_ok
            && settings.material
            && !self.materializes_output(total)
        {
            material = Some(self.material_path(total)?);
        }
        for &held_outer in &outer_rel.paths[..usize::from(outer_rel.count)] {
            // An outer path reading the inner relation's values cannot be.
            if self.path(held_outer).param & inputs.inner != 0 {
                continue;
            }
            let mut outer = held_outer;
            if inputs.jointype == JoinType::UniqueOuter {
                if outer != outer_rel.cheapest_total {
                    continue;
                }
                outer = self.unique_of(inputs.outer, inputs.special)?;
            }
            let keys = self.join_keys(joined, inputs.path_jointype(), &self.path(outer).keys);
            if inputs.jointype == JoinType::UniqueInner {
                self.try_nestloop(joined, inputs, outer, inner_total.expect("unique"), keys)?;
            } else if nestloop_ok {
                // The cheapest inner path of each parameterization, the
                // unparameterized one first.
                for &inner in &inner_rel.parameterized[..usize::from(inner_rel.parameterized_count)]
                {
                    self.try_nestloop(joined, inputs, outer, inner, keys)?;
                    if let Some(memoize) = self.memoize_path(inputs, outer, inner)? {
                        self.try_nestloop(joined, inputs, outer, memoize, keys)?;
                    }
                }
                if let Some(material) = material {
                    self.try_nestloop(joined, inputs, outer, material, keys)?;
                }
            }
            if inputs.jointype == JoinType::UniqueOuter {
                continue;
            }
            let Some(inner_total) = inner_total else {
                continue;
            };
            self.generate_mergejoin_paths(joined, inputs, outer, inner_total, keys, use_all)?;
        }
        Ok(())
    }

    /// `calc_joinrel_size_estimate` of a join of `outer` and `inner` rows
    /// by `restrict`, with `get_foreign_key_join_selectivity`: a foreign
    /// key whose every column the clauses equate makes each referencing
    /// row match one referenced row, and its clauses leave the estimate.
    fn join_size(
        &mut self,
        joined: Relids,
        (outer, inner): (Relids, Relids),
        outer_rows: f64,
        inner_rows: f64,
        special: Option<u8>,
        restrict: &ClauseList,
    ) -> Result<f64, SqlError> {
        let jointype = match special.map(|id| self.special(id).kind) {
            None => JoinType::Inner,
            Some(SpecialKind::Left) => JoinType::Left,
            Some(SpecialKind::Full) => JoinType::Full,
            Some(SpecialKind::Semi) => JoinType::Semi,
            Some(SpecialKind::Anti) => JoinType::Anti,
        };
        let mut kept = *restrict;
        let foreign = self.foreign_key_selectivity(outer, inner, jointype, &mut kept);
        let mode = if jointype == JoinType::Inner {
            Selectivity::Inner
        } else {
            Selectivity::Outer(special)
        };
        let (join, pushed) = if jointype.is_outer() {
            let (mut join, mut pushed) = (ClauseList::EMPTY, ClauseList::EMPTY);
            for &id in kept.as_slice() {
                let list = if self.clause(id).is_pushed_down(joined) {
                    &mut pushed
                } else {
                    &mut join
                };
                let _ = list.push(id);
            }
            (join, pushed)
        } else {
            (kept, ClauseList::EMPTY)
        };
        let join_selectivity = self.clauselist_selectivity(&join, mode)?;
        let pushed_selectivity = self.clauselist_selectivity(&pushed, mode)?;
        let rows = match jointype {
            JoinType::Inner => outer_rows * inner_rows * foreign * join_selectivity,
            JoinType::Left => {
                (outer_rows * inner_rows * foreign * join_selectivity).max(outer_rows)
                    * pushed_selectivity
            }
            JoinType::Full => {
                (outer_rows * inner_rows * foreign * join_selectivity)
                    .max(outer_rows)
                    .max(inner_rows)
                    * pushed_selectivity
            }
            JoinType::Semi => outer_rows * foreign * join_selectivity,
            _ => outer_rows * (1.0 - foreign * join_selectivity) * pushed_selectivity,
        };
        Ok(clamp_row_est(rows))
    }

    /// `get_foreign_key_join_selectivity`: removes the clauses each
    /// matching foreign key accounts for from `kept`.
    fn foreign_key_selectivity(
        &self,
        outer: Relids,
        inner: Relids,
        jointype: JoinType,
        kept: &mut ClauseList,
    ) -> f64 {
        let mut selectivity = 1.0;
        for key in self.problem.foreign_keys {
            let (referencing, referenced) = (1 << key.referencing, 1 << key.referenced);
            let referenced_outer = if outer & referencing != 0 && inner & referenced != 0 {
                false
            } else if outer & referenced != 0 && inner & referencing != 0 {
                true
            } else {
                continue;
            };
            if matches!(jointype, JoinType::Semi | JoinType::Anti)
                && (referenced_outer || inner.count_ones() != 1)
            {
                continue;
            }
            let classes = &key.classes[..usize::from(key.count)];
            let mut remaining = ClauseList::EMPTY;
            let mut removed = 0;
            for &id in kept.as_slice() {
                let matches = match self.clause(id).form {
                    ClauseForm::Equality { class, .. } => classes.contains(&class),
                    _ => key.clauses.contains(id),
                };
                if matches {
                    removed += 1;
                } else {
                    let _ = remaining.push(id);
                }
            }
            let matched_classes = classes
                .iter()
                .filter(|&&class| class != NO_KEY_CLASS)
                .count();
            if removed == 0 || removed != matched_classes + key.clauses.as_slice().len() {
                continue;
            }
            *kept = remaining;
            selectivity *= if matches!(jointype, JoinType::Semi | JoinType::Anti) {
                key.referenced_rows / key.referenced_tuples
            } else {
                1.0 / key.referenced_tuples
            };
        }
        selectivity.clamp(0.0, 1.0)
    }

    /// `innerrel_is_unique`: the inner relation is one base relation with
    /// a unique index whose every key column a join clause or a constant
    /// equality fixes (`relation_has_unique_index_ext`).
    fn inner_unique(
        &self,
        joined: Relids,
        outer: Relids,
        inner: Relids,
        jointype: JoinType,
        restrict: &ClauseList,
    ) -> bool {
        if restrict.len == 0 || inner.count_ones() != 1 || inner & !BASE_RELIDS != 0 {
            return false;
        }
        let base = &self.problem.rels[inner.trailing_zeros() as usize];
        if base.unique.is_empty() {
            return false;
        }
        let mut matched = base.fixed;
        let mut any = false;
        for &id in restrict.as_slice() {
            if jointype.is_outer() && self.clause(id).is_pushed_down(joined) {
                continue;
            }
            let Some(outer_is_left) = self.outer_is_left(id, outer, inner) else {
                continue;
            };
            let (left, right, ..) = self.sides(id).expect("mergeable");
            let member = if outer_is_left { right } else { left };
            any = true;
            if member.column < 64 {
                matched |= 1 << member.column;
            }
        }
        any && base.unique.iter().any(|&keys| keys & !matched == 0)
    }

    /// `compute_semi_anti_join_factors`: the fraction of outer rows with a
    /// match (the clauses' semijoin selectivity, which an inner or outer
    /// join's `sjinfo` answers with its own) and the matches each has.
    fn semi_factors(
        &mut self,
        joined: Relids,
        inner: Relids,
        jointype: JoinType,
        special: Option<u8>,
        restrict: &ClauseList,
    ) -> Result<(f64, f64), SqlError> {
        let mut quals = ClauseList::EMPTY;
        for &id in restrict.as_slice() {
            if !jointype.is_outer() || !self.clause(id).is_pushed_down(joined) {
                let _ = quals.push(id);
            }
        }
        let matched = self.clauselist_selectivity(&quals, Selectivity::Outer(special))?;
        let selectivity = self.clauselist_selectivity(&quals, Selectivity::Inner)?;
        let count = if matched > 0.0 {
            (selectivity * self.rel(inner).rows / matched).max(1.0)
        } else {
            1.0
        };
        Ok((matched, count))
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
        if !inputs.inner_unique && matches!(inputs.jointype, JoinType::Semi | JoinType::Anti) {
            return Ok(None);
        }
        let enforced = self.enforced(inner);
        if enforced.len == 0
            || (inputs.inner_unique
                && inputs
                    .restrict
                    .as_slice()
                    .iter()
                    .any(|&id| !self.holds_serial(&enforced, id)))
        {
            return Ok(None);
        }
        // The cache key: each moved clause's outer side, once.
        let mut keys = [Member::plain(0, 0); MAX_KEYS];
        let mut key_count = 0usize;
        let mut width = 0.0;
        let mut binary = false;
        for &id in enforced.as_slice() {
            binary |= self.sides(id).is_none();
            let Some((left, right)) = self
                .sides(id)
                .map(|(left, right, ..)| (left, right))
                .or(self.clause(id).operands)
            else {
                return Ok(None);
            };
            let key = if left.relids() & !inputs.outer == 0 && right.relids() & !inputs.inner == 0 {
                left
            } else if right.relids() & !inputs.outer == 0 && left.relids() & !inputs.inner == 0 {
                right
            } else {
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
                binary,
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
            PathKind::Dummy => ClauseList::EMPTY,
            PathKind::Scan { rel, path } => {
                self.problem.rels[usize::from(rel)].paths[usize::from(path)].enforced
            }
            // These share their input's `param_info`, whose `ppi_serials`
            // only a base relation's holds.
            PathKind::Material { input }
            | PathKind::Memoize { input, .. }
            | PathKind::Unique { input, .. } => match self.path(input).kind {
                PathKind::Scan { .. } => self.enforced(input),
                _ => ClauseList::EMPTY,
            },
            PathKind::NestLoop {
                outer,
                inner,
                clauses,
                ..
            }
            | PathKind::HashJoin {
                outer,
                inner,
                clauses,
                ..
            }
            | PathKind::MergeJoin {
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
        }
    }

    /// `get_joinrel_parampathinfo`'s clauses of a join parameterized by
    /// `required` whose inputs read `outer_param` and `inner_param`, which
    /// neither input can apply: the join's own join clauses movable into
    /// it; then per class, its first member among `required` equated with
    /// its first in the join; then, for each class whose clause the inner
    /// path could apply, its first member among `required` equated with
    /// its first in the outer relation, unless the outer path applies it.
    fn moved_join_clauses(
        &mut self,
        joined: Relids,
        inputs: &JoinInputs,
        (outer_param, inner_param): (Relids, Relids),
        required: Relids,
    ) -> Result<ClauseList, SqlError> {
        // `join_clause_is_movable_into` relations `current` reading
        // `available`, `None` for an unparameterized input.
        let movable =
            |relids: Relids, outer_relids: Relids, current: Relids, available: Option<Relids>| {
                available.is_some_and(|available| {
                    relids & !available == 0 && relids & current != 0 && outer_relids & current == 0
                })
            };
        let join_and_req = Some(joined | required);
        let outer_and_req = (outer_param != 0).then_some(inputs.outer | outer_param);
        let inner_and_req = (inner_param != 0).then_some(inputs.inner | inner_param);
        let mut moved = ClauseList::EMPTY;
        for &id in self.rel(joined).joininfo.as_slice() {
            let clause = self.clause(id);
            let (relids, outer_relids) = (clause.relids, clause.outer_relids);
            if movable(relids, outer_relids, joined, join_and_req)
                && !movable(relids, outer_relids, inputs.outer, outer_and_req)
                && !movable(relids, outer_relids, inputs.inner, inner_and_req)
            {
                moved.push(id).ok_or_else(too_many)?;
            }
        }
        let mut dropped = [0u8; MAX_JOIN_CLAUSES];
        let mut dropped_count = 0;
        let first = |members: &[Member], relids: Relids| {
            members
                .iter()
                .position(|member| member.relids() & !relids == 0)
        };
        for class_id in 0..self.problem.classes.len() {
            let class = self.problem.classes[class_id];
            if class.has_const || class.count <= 1 {
                continue;
            }
            let members = class.members();
            let (Some(outside), Some(within)) = (first(members, required), first(members, joined))
            else {
                continue;
            };
            let relids = members[outside].relids() | members[within].relids();
            if movable(relids, 0, inputs.outer, outer_and_req) {
                continue;
            }
            if movable(relids, 0, inputs.inner, inner_and_req) {
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
            let relids = members[outside].relids() | members[outer].relids();
            if movable(relids, 0, inputs.outer, outer_and_req) {
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

    /// `generate_mergejoin_paths`.
    fn generate_mergejoin_paths(
        &mut self,
        joined: Relids,
        inputs: &JoinInputs,
        outer: u32,
        inner_total: u32,
        keys: Keys,
        use_all: bool,
    ) -> Result<(), SqlError> {
        let outer_keys = self.path(outer).keys;
        let merge = self.mergeclauses_for_outer_keys(&outer_keys, inputs);
        if merge.len == 0 && inputs.jointype != JoinType::Full {
            return Ok(());
        }
        if use_all && merge.len != inputs.mergeable.len {
            return Ok(());
        }
        let inner_keys = self.inner_keys_for_merge(&merge, inputs, &outer_keys);
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
        if inputs.jointype == JoinType::UniqueInner {
            return Ok(());
        }
        let (mut cheapest_total, mut cheapest_startup) =
            if inner_keys.contained_in(&self.path(inner_total).keys) {
                (Some(inner_total), Some(inner_total))
            } else {
                (None, None)
            };
        let inner_rel = self.rel(inputs.inner);
        let count_all = usize::from(inner_keys.len);
        for count in (1..=count_all).rev() {
            let trial = inner_keys.truncated(count);
            let mut new_clauses = None;
            let total = self.cheapest_for_keys(&inner_rel, &trial, false);
            if let Some(path) = total
                && cheapest_total
                    .is_none_or(|held| compare_costs(&self.path(path), &self.path(held), false) < 0)
            {
                let clauses = if count < count_all {
                    self.trim_mergeclauses(&merge, &trial, inputs)
                } else {
                    merge
                };
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
                        if count < count_all {
                            self.trim_mergeclauses(&merge, &trial, inputs)
                        } else {
                            merge
                        }
                    });
                    self.try_mergejoin(joined, inputs, outer, path, keys, clauses, None, None)?;
                }
                cheapest_startup = Some(path);
            }
            if use_all {
                break;
            }
        }
        Ok(())
    }

    /// `trim_mergeclauses_for_inner_pathkeys`.
    fn trim_mergeclauses(
        &self,
        merge: &ClauseList,
        keys: &Keys,
        inputs: &JoinInputs,
    ) -> ClauseList {
        let mut trimmed = ClauseList::EMPTY;
        let keys = keys.as_slice();
        let Some(&first) = keys.first() else {
            return trimmed;
        };
        let (mut current, mut next) = (first & KEY_CLASS, 1);
        let mut matched = false;
        for &id in merge.as_slice() {
            let (_, class) = self.merge_classes(id, inputs.outer, inputs.inner);
            if class != current {
                if !matched || next == keys.len() {
                    break;
                }
                current = keys[next] & KEY_CLASS;
                next += 1;
            }
            if class == current {
                let _ = trimmed.push(id);
                matched = true;
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
    /// `final_cost_nestloop`.
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
        if let Some(special) = inputs.special {
            let oj = self.special(special).oj;
            if oj != 0 && (inner_path.param | outer_path.param) & oj != 0 {
                return Ok(());
            }
        }
        // `calc_nestloop_required_outer`: what the join still reads from
        // other relations, which the search allows from its parameter
        // sources or where `allow_star_schema_join` lets it build a
        // parameterized join.
        let required = (outer_path.param | inner_path.param) & !inputs.outer;
        let mut moved = ClauseList::EMPTY;
        if required != 0 {
            let star =
                inner_path.param & inputs.outer != 0 && inner_path.param & !inputs.outer != 0;
            if required & inputs.param_source == 0 && !star {
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
            if !self.holds_serial(&enforced, id) {
                clauses.push(id).ok_or_else(too_many)?;
            }
        }
        let param_rows =
            self.param_join_rows(joined, inputs, (outer_path, inner_path), required, &clauses)?;
        let (rescan_startup, rescan_total) = self.rescan_cost(inner);
        let mut startup = outer_path.startup + inner_path.startup;
        let mut run = outer_path.total - outer_path.startup;
        if outer_path.rows > 1.0 {
            run += (outer_path.rows - 1.0) * rescan_startup;
        }
        let inner_run = inner_path.total - inner_path.startup;
        let inner_rescan_run = rescan_total - rescan_startup;
        if !inputs.first_match() {
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
        // `final_cost_nestloop`: a semi or anti join's or a unique inner
        // relation's scan stops at the first match.
        let tuples = if inputs.first_match() {
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
                    jointype: inputs.path_jointype(),
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

    /// `ppi_rows` of a join parameterized by `required` evaluating
    /// `clauses`: the first estimate for the join's parameterization
    /// (`get_parameterized_joinrel_size`), or `None` unparameterized.
    fn param_join_rows(
        &mut self,
        joined: Relids,
        inputs: &JoinInputs,
        (outer_path, inner_path): (Path, Path),
        required: Relids,
        clauses: &ClauseList,
    ) -> Result<Option<f64>, SqlError> {
        if required == 0 {
            return Ok(None);
        }
        if let Some(&(_, _, rows)) = self
            .param_rows
            .borrowed_slice()
            .iter()
            .find(|(relids, param, _)| *relids == joined && *param == required)
        {
            return Ok(Some(rows));
        }
        let rows = self
            .join_size(
                joined,
                (inputs.outer, inputs.inner),
                outer_path.rows,
                inner_path.rows,
                inputs.special,
                clauses,
            )?
            .min(self.rel(joined).rows);
        self.param_rows
            .push((joined, required, rows))
            .map_err(|_| arena_full())?;
        Ok(Some(rows))
    }

    /// `calc_non_nestloop_required_outer` of a hash or merge join, and the
    /// clauses it evaluates: those its parameterization moves into it
    /// (`get_joinrel_parampathinfo`), then the restriction list. `None`
    /// when the parameterization is not one the join may take.
    fn non_nestloop_clauses(
        &mut self,
        joined: Relids,
        inputs: &JoinInputs,
        (outer_path, inner_path): (Path, Path),
    ) -> Result<Option<(Relids, ClauseList)>, SqlError> {
        let required = outer_path.param | inner_path.param;
        if required == 0 {
            return Ok(Some((0, inputs.restrict)));
        }
        if required & inputs.param_source == 0 {
            return Ok(None);
        }
        let mut clauses = self.moved_join_clauses(
            joined,
            inputs,
            (outer_path.param, inner_path.param),
            required,
        )?;
        for &id in inputs.restrict.as_slice() {
            clauses.push(id).ok_or_else(too_many)?;
        }
        Ok(Some((required, clauses)))
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
    /// `final_cost_mergejoin`. `outer_sort` and `inner_sort` are the keys
    /// to sort by, or `None` for an input already in order.
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
        let Some((required, restrict)) =
            self.non_nestloop_clauses(joined, inputs, (outer_path, inner_path))?
        else {
            return Ok(());
        };
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
        // Each merge clause sorts in its outer pathkey's direction.
        let outer_keys = outer_sort.unwrap_or(outer_path.keys);
        let (mut descending, mut nulls_first) = (0u32, 0u32);
        for (at, &id) in merge.as_slice().iter().enumerate() {
            let flags = self.merge_direction(id, inputs, &outer_keys);
            descending |= u32::from(flags & KEY_DESCENDING != 0) << at;
            nulls_first |= u32::from(flags & KEY_NULLS_FIRST != 0) << at;
        }
        // `mergejoinscansel` of the first clause in its direction, which a
        // full join and an outer join's preserved side do not use.
        let mut range = [0.0, 1.0, 0.0, 1.0];
        if let Some(&first) = merge.as_slice().first()
            && inputs.jointype != JoinType::Full
        {
            let (left, right, ..) = self.sides(first).expect("a merge clause");
            let Some(estimate) = self.statistics.merge_range(
                left,
                right,
                descending & 1 != 0,
                nulls_first & 1 != 0,
            )?
            else {
                self.outside = Some("a merge join range estimate outside the model");
                return Ok(());
            };
            range = if self.outer_is_left(first, inputs.outer, inputs.inner) == Some(true) {
                estimate
            } else {
                [estimate[2], estimate[3], estimate[0], estimate[1]]
            };
            match inputs.jointype {
                JoinType::Left | JoinType::Anti => {
                    range[0] = 0.0;
                    range[1] = 1.0;
                }
                JoinType::Right | JoinType::RightAnti => {
                    range[2] = 0.0;
                    range[3] = 1.0;
                }
                _ => {}
            }
        }
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
        let other_cost = self.qual_cost(&restrict) - merge_cost;
        let selectivity = self.clauselist_selectivity(&merge, Selectivity::Inner)?;
        let merged = clamp_row_est(selectivity * outer_path.rows * inner_path.rows);
        // A semi or anti join, or a unique inner relation, every clause of
        // which merges needs no mark.
        let skip_mark_restore = inputs.first_match() && restrict.len == merge.len;
        let rescanned = if skip_mark_restore || matches!(outer_path.kind, PathKind::Unique { .. }) {
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
        let param_rows = self.param_join_rows(
            joined,
            inputs,
            (outer_path, inner_path),
            required,
            &restrict,
        )?;
        let rel = self.rel(joined);
        self.add_path(
            joined,
            Path {
                kind: PathKind::MergeJoin {
                    jointype: inputs.path_jointype(),
                    outer,
                    inner,
                    clauses: restrict,
                    merge,
                    outer_sort: outer_sort.unwrap_or(Keys::NONE),
                    inner_sort: inner_sort.unwrap_or(Keys::NONE),
                    materialize_inner,
                    inner_unique: inputs.inner_unique,
                    descending,
                    nulls_first,
                },
                relids: joined,
                rows: param_rows.unwrap_or(rel.rows),
                width: rel.width,
                startup,
                total: startup + run,
                disabled,
                keys,
                param: required,
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

    /// `hash_inner_and_outer`.
    fn hash_inner_and_outer(
        &mut self,
        joined: Relids,
        inputs: &JoinInputs,
    ) -> Result<(), SqlError> {
        let mut hash = ClauseList::EMPTY;
        for &id in inputs.restrict.as_slice() {
            if inputs.jointype.is_outer() && self.clause(id).is_pushed_down(joined) {
                continue;
            }
            if self.outer_is_left(id, inputs.outer, inputs.inner).is_some() {
                let _ = hash.push(id);
            }
        }
        if hash.len == 0 {
            return Ok(());
        }
        let outer_rel = self.rel(inputs.outer);
        let inner_rel = self.rel(inputs.inner);
        let (startup_outer, total_outer, total_inner) = (
            outer_rel.cheapest_startup,
            outer_rel.cheapest_total,
            inner_rel.cheapest_total,
        );
        if self.path(total_outer).param & inputs.inner != 0
            || self.path(total_inner).param & inputs.outer != 0
        {
            return Ok(());
        }
        match inputs.jointype {
            JoinType::UniqueOuter => {
                let outer = self.unique_of(inputs.outer, inputs.special)?;
                return self.try_hashjoin(joined, inputs, outer, total_inner, hash);
            }
            JoinType::UniqueInner => {
                let inner = self.unique_of(inputs.inner, inputs.special)?;
                self.try_hashjoin(joined, inputs, total_outer, inner, hash)?;
                if startup_outer != total_outer {
                    self.try_hashjoin(joined, inputs, startup_outer, inner, hash)?;
                }
                return Ok(());
            }
            _ => {}
        }
        self.try_hashjoin(joined, inputs, startup_outer, total_inner, hash)?;
        for &outer in &outer_rel.parameterized[..usize::from(outer_rel.parameterized_count)] {
            if self.path(outer).param & inputs.inner != 0 {
                continue;
            }
            for &inner in &inner_rel.parameterized[..usize::from(inner_rel.parameterized_count)] {
                if self.path(inner).param & inputs.outer != 0 {
                    continue;
                }
                if outer == startup_outer && inner == total_inner {
                    continue;
                }
                self.try_hashjoin(joined, inputs, outer, inner, hash)?;
            }
        }
        Ok(())
    }

    /// `estimate_hash_bucket_stats` of a hash clause's inner side, as the
    /// clause caches its first estimate.
    fn bucket(
        &mut self,
        id: u8,
        inner: Member,
        left: bool,
        buckets: f64,
    ) -> Result<(f64, f64), SqlError> {
        let side = usize::from(!left);
        if let Some(held) = self.buckets[usize::from(id)][side] {
            return Ok(held);
        }
        let estimate = self.statistics.bucket(inner, buckets)?;
        self.buckets[usize::from(id)][side] = Some(estimate);
        Ok(estimate)
    }

    /// `try_hashjoin_path` with `initial_cost_hashjoin` and
    /// `final_cost_hashjoin`.
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
        let Some((required, restrict)) =
            self.non_nestloop_clauses(joined, inputs, (outer_path, inner_path))?
        else {
            return Ok(());
        };
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
        // A unique-ified inner relation hashes well.
        let unique_inner = matches!(inner_path.kind, PathKind::Unique { .. });
        if unique_inner {
            bucket_size = 1.0 / virtual_buckets;
            mcv_frequency = 0.0;
        }
        for &id in hash.as_slice().iter().filter(|_| !unique_inner) {
            let (left, right, ..) = self.sides(id).expect("a hash clause");
            let outer_is_left = self
                .outer_is_left(id, inputs.outer, inputs.inner)
                .expect("a hash clause joins the inputs");
            let inner_member = if outer_is_left { right } else { left };
            let (frequency, size) =
                self.bucket(id, inner_member, !outer_is_left, virtual_buckets)?;
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
        let other_cost = self.qual_cost(&restrict) - hash_cost;
        let tuples = if inputs.first_match() {
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
            if inputs.jointype == JoinType::Anti {
                outer_path.rows - matched
            } else {
                matched
            }
        } else {
            run += hash_cost * outer_path.rows * clamp_row_est(inner_path.rows * bucket_size) * 0.5;
            let selectivity = self.clauselist_selectivity(&hash, Selectivity::Inner)?;
            clamp_row_est(selectivity * outer_path.rows * inner_path.rows)
        };
        run += (costs.cpu_tuple + other_cost) * tuples;
        let param_rows = self.param_join_rows(
            joined,
            inputs,
            (outer_path, inner_path),
            required,
            &restrict,
        )?;
        let rel = self.rel(joined);
        self.add_path(
            joined,
            Path {
                kind: PathKind::HashJoin {
                    jointype: inputs.path_jointype(),
                    outer,
                    inner,
                    clauses: restrict,
                    hash,
                    buckets: sizing.buckets,
                    batches: sizing.batches,
                },
                relids: joined,
                rows: param_rows.unwrap_or(rel.rows),
                width: rel.width,
                startup,
                total: startup + run,
                disabled,
                keys: Keys::NONE,
                param: required,
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

/// A join's clauses as its plan evaluates them (`extract_actual_join_clauses`
/// and `order_qual_clauses`): an outer join's matching conditions apart from
/// the filters pushed down to it, each list cheapest first, without the
/// clauses its hash or merge keys apply (`keyed`).
pub(crate) fn split_join_clauses(
    all: &[Clause],
    clauses: &ClauseList,
    relids: Relids,
    jointype: JoinType,
    keyed: &ClauseList,
) -> (ClauseList, ClauseList) {
    let (mut join, mut other) = (ClauseList::EMPTY, ClauseList::EMPTY);
    for &id in clauses.as_slice() {
        if keyed.contains(id) {
            continue;
        }
        let list = if jointype.is_outer() && all[usize::from(id)].is_pushed_down(relids) {
            &mut other
        } else {
            &mut join
        };
        let _ = list.push(id);
    }
    for list in [&mut join, &mut other] {
        let len = usize::from(list.len);
        // A stable insertion sort by cost, as `order_qual_clauses` sorts.
        for at in 1..len {
            let mut position = at;
            while position > 0
                && all[usize::from(list.ids[position])].cost
                    < all[usize::from(list.ids[position - 1])].cost
            {
                list.ids.swap(position, position - 1);
                position -= 1;
            }
        }
    }
    (join, other)
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

/// `add_path` of `new`, numbered `id`, into the pathlist `held[..count]`,
/// kept in order of total cost; returns the new count and whether `new`
/// was kept. A parameterized path's
/// pathkeys do not count, and one path dominates another only when it
/// needs no more of the other relations (`bms_subset_compare`). A
/// parameterized path never considers its startup cost.
fn add_to_pathlist(
    held: &mut [u32; MAX_REL_PATHS],
    count: usize,
    (id, new): (u32, Path),
    path: &dyn Fn(u32) -> Path,
    consider_startup: bool,
) -> Result<(usize, bool), SqlError> {
    let consider_startup = consider_startup && new.param == 0;
    let mut kept = [0u32; MAX_REL_PATHS];
    let mut kept_count = 0usize;
    let mut accept = true;
    let mut insert_at = 0usize;
    let mut position = 0;
    while position < count {
        let old_id = held[position];
        let old = path(old_id);
        position += 1;
        let mut remove_old = false;
        let costs = compare_fuzzily(&new, &old, FUZZ, consider_startup);
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
                                } else if compare_fuzzily(
                                    &new,
                                    &old,
                                    1.000_000_000_1,
                                    consider_startup,
                                ) == CostOrder::Better1
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
        kept.copy_within(insert_at..kept_count, insert_at + 1);
        kept[insert_at] = id;
        kept_count += 1;
    }
    *held = kept;
    Ok((kept_count, accept))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CostOrder {
    Better1,
    Better2,
    Equal,
    Different,
}

/// `compare_path_costs_fuzzily`: with `consider_startup` (a query with a
/// `LIMIT`) a path cheaper to start but dearer in total differs.
fn compare_fuzzily(new: &Path, old: &Path, fuzz: f64, consider_startup: bool) -> CostOrder {
    if new.disabled != old.disabled {
        return if new.disabled < old.disabled {
            CostOrder::Better1
        } else {
            CostOrder::Better2
        };
    }
    if new.total > old.total * fuzz {
        if consider_startup && old.startup > new.startup * fuzz {
            return CostOrder::Different;
        }
        return CostOrder::Better2;
    }
    if old.total > new.total * fuzz {
        if consider_startup && new.startup > old.startup * fuzz {
            return CostOrder::Different;
        }
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
            incremental_sort: true,
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
        fn semi(&self, _: Member, _: Member, _: f64) -> Result<f64, SqlError> {
            Ok(1.0)
        }
        fn bucket(&self, _: Member, buckets: f64) -> Result<(f64, f64), SqlError> {
            Ok((0.0, (1.0 / 1000.0_f64).max(1.0 / buckets)))
        }
        fn merge_range(
            &self,
            _: Member,
            _: Member,
            _: bool,
            _: bool,
        ) -> Result<Option<[f64; 4]>, SqlError> {
            Ok(Some([0.0, 1.0, 0.0, 1.0]))
        }
        fn groups(&self, _: &[Member], calls: f64) -> Result<f64, SqlError> {
            Ok(calls)
        }
        fn num_groups(&self, _: &[Member], rows: f64) -> Result<f64, SqlError> {
            Ok(rows)
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
            members: [Member::plain(0, 0); MAX_MEMBERS],
            count: 2,
            has_const: false,
        };
        class.members[1] = Member::plain(1, 0);
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
            specials: &[],
            subproblems: &[Subproblem {
                items: [1, 2, 0, 0, 0, 0, 0, 0],
                count: 2,
            }],
            clauses: &[Clause {
                form: ClauseForm::Equality {
                    class: 0,
                    left: 0,
                    right: 1,
                },
                relids: 3,
                required: 3,
                pushed_down: true,
                clone: CloneKind::None,
                incompatible: 0,
                cost: 0.0025,
                selectivity: None,
                outer_selectivity: None,
                outer_relids: 0,
                operands: None,
                other: OtherShape::Plain,
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
            removed: 0,
            upper: UpperProblem {
                sort: Keys::NONE,
                limit: None,
                width: 20.0,
                target_cost: 0.0,
                group: None,
            },
            dummy: false,
            false_relids: 0,
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
