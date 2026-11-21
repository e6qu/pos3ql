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

use crate::mem::arena::{Arena, ArenaChunks, ArenaList};
use crate::sql::eval::SqlError;

use super::{Costs, clamp_row_est};

mod geqo;
pub(crate) use geqo::GeqoSettings;

/// Base relations of one modeled query. Its join problems are split by
/// `join_collapse_limit` and `from_collapse_limit` and each is searched
/// exhaustively or, from `geqo_threshold` on, genetically.
pub(crate) const MAX_JOIN_RELS: usize = 128;
/// Members of one equivalence class, numbered by `u8`; the members
/// themselves live in the statement arena.
pub(crate) const MAX_MEMBERS: usize = u8::MAX as usize;
/// Clauses one search holds, numbered by [`ClauseId`]; the clauses live in
/// the statement arena.
pub(crate) const MAX_CLAUSES: usize = u16::MAX as usize;
/// A clause's number in the search's clause list.
pub(crate) type ClauseId = u16;
/// Join clauses of one join or relation, and sort keys of one path.
pub(crate) const MAX_JOIN_CLAUSES: usize = 32;
pub(crate) const MAX_KEYS: usize = 8;
/// Paths kept per relation by `add_path`.
const MAX_REL_PATHS: usize = 32;
/// Paths and relations per chunk of the search's lists.
const SEARCH_CHUNK: usize = 128;

/// A set of base relations, by bit, and of the outer joins above them
/// (PostgreSQL's join relids), each at [`OJ_SHIFT`] past its number.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default, Debug)]
pub(crate) struct Relids([u64; RELIDS_WORDS]);
const RELIDS_WORDS: usize = 2 * MAX_JOIN_RELS / 64;
/// The first outer join's bit.
pub(crate) const OJ_SHIFT: u32 = MAX_JOIN_RELS as u32;
/// The base relations' bits of a [`Relids`].
pub(crate) const BASE_RELIDS: Relids = Relids::below(OJ_SHIFT);

impl Relids {
    pub(crate) const EMPTY: Self = Self([0; RELIDS_WORDS]);

    /// The set of member `bit` alone.
    pub(crate) const fn bit(bit: u32) -> Self {
        let mut words = [0; RELIDS_WORDS];
        words[(bit / 64) as usize] = 1 << (bit % 64);
        Self(words)
    }

    /// Base relation `rel` alone.
    pub(crate) fn of(rel: impl RelNumber) -> Self {
        Self::bit(rel.number())
    }

    /// The members below `bit`.
    pub(crate) const fn below(bit: u32) -> Self {
        let mut words = [0; RELIDS_WORDS];
        let mut at = 0;
        while at < RELIDS_WORDS {
            let start = at as u32 * 64;
            words[at] = if bit >= start + 64 {
                u64::MAX
            } else if bit > start {
                (1 << (bit - start)) - 1
            } else {
                0
            };
            at += 1;
        }
        Self(words)
    }

    /// The set of the low 64 members `mask` holds.
    #[cfg(test)]
    pub(crate) const fn from_mask(mask: u64) -> Self {
        let mut words = [0; RELIDS_WORDS];
        words[0] = mask;
        Self(words)
    }

    pub(crate) const fn is_empty(self) -> bool {
        let mut at = 0;
        while at < RELIDS_WORDS {
            if self.0[at] != 0 {
                return false;
            }
            at += 1;
        }
        true
    }

    /// `bms_overlap`.
    pub(crate) fn overlaps(self, other: Self) -> bool {
        !(self & other).is_empty()
    }

    pub(crate) const fn contains(self, bit: u32) -> bool {
        self.0[(bit / 64) as usize] & 1 << (bit % 64) != 0
    }

    /// `bms_num_members`.
    pub(crate) fn count_ones(self) -> u32 {
        self.0.iter().map(|word| word.count_ones()).sum()
    }

    /// The lowest member; past every member when empty.
    pub(crate) fn trailing_zeros(self) -> u32 {
        let mut zeros = 0;
        for word in self.0 {
            if word != 0 {
                return zeros + word.trailing_zeros();
            }
            zeros += 64;
        }
        zeros
    }

    /// The members, lowest first.
    pub(crate) fn members(self) -> impl Iterator<Item = u32> {
        let mut rest = self;
        core::iter::from_fn(move || {
            if rest.is_empty() {
                return None;
            }
            let bit = rest.trailing_zeros();
            rest &= !Self::bit(bit);
            Some(bit)
        })
    }

    /// The base relations, as relation numbers.
    pub(crate) fn rels(self) -> impl Iterator<Item = u8> {
        (self & BASE_RELIDS).members().map(|bit| bit as u8)
    }

    /// A hash of the set for the join relation table.
    fn hash(self) -> u64 {
        self.0.iter().fold(0u64, |hash, &word| {
            (hash ^ word)
                .wrapping_mul(0x9E37_79B9_7F4A_7C15)
                .rotate_left(29)
        })
    }
}

/// A base relation's or outer join's number.
pub(crate) trait RelNumber {
    fn number(self) -> u32;
}

impl RelNumber for u8 {
    fn number(self) -> u32 {
        u32::from(self)
    }
}

impl RelNumber for u32 {
    fn number(self) -> u32 {
        self
    }
}

impl RelNumber for usize {
    fn number(self) -> u32 {
        u32::try_from(self).expect("a relation number")
    }
}

impl core::ops::BitOr for Relids {
    type Output = Self;
    fn bitor(self, other: Self) -> Self {
        Self(core::array::from_fn(|at| self.0[at] | other.0[at]))
    }
}

impl core::ops::BitAnd for Relids {
    type Output = Self;
    fn bitand(self, other: Self) -> Self {
        Self(core::array::from_fn(|at| self.0[at] & other.0[at]))
    }
}

impl core::ops::Not for Relids {
    type Output = Self;
    fn not(self) -> Self {
        Self(self.0.map(|word| !word))
    }
}

impl core::ops::BitOrAssign for Relids {
    fn bitor_assign(&mut self, other: Self) {
        *self = *self | other;
    }
}

impl core::ops::BitAndAssign for Relids {
    fn bitand_assign(&mut self, other: Self) {
        *self = *self & other;
    }
}

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
    /// The other base relations an expression member of several
    /// relations reads (`a.x + b.y`, `rel` its first): the join of them
    /// all computes it.
    pub(crate) reads: Relids,
}

impl Member {
    /// The column as its own relation's scan reads it.
    pub(crate) const fn plain(rel: u8, column: u16) -> Self {
        Self {
            rel,
            column,
            nulling: Relids::EMPTY,
            reads: Relids::EMPTY,
        }
    }

    /// Expression `id` reading the base relations `relids`, several of
    /// them, which their join computes.
    pub(crate) fn several(relids: Relids, id: u16) -> Self {
        let rel = relids.trailing_zeros() as u8;
        Self {
            rel,
            column: EXPRESSION_COLUMN | id,
            nulling: Relids::EMPTY,
            reads: relids & !Relids::of(rel),
        }
    }

    /// Expression `id` of the query's [`EXPRESSION_COLUMN`] numbering, read
    /// at relation `rel`'s scan.
    pub(crate) const fn expression(rel: u8, id: u16) -> Self {
        Self::plain(rel, EXPRESSION_COLUMN | id)
    }

    /// The query expression the member is, when it is no plain column.
    pub(crate) const fn expression_id(&self) -> Option<usize> {
        if self.column & EXPRESSION_COLUMN != 0 {
            Some((self.column & !EXPRESSION_COLUMN) as usize)
        } else {
            None
        }
    }

    /// `em_relids`.
    pub(crate) fn relids(&self) -> Relids {
        Relids::of(self.rel) | self.reads | self.nulling
    }

    /// The base relations it reads.
    pub(crate) fn base_relids(&self) -> Relids {
        Relids::of(self.rel) | self.reads
    }
}

/// The bit of a [`Member::column`] that makes it an expression of its
/// relation's columns rather than a column: an `EquivalenceMember` whose
/// `em_expr` is no `Var`, such as `a.k + 1`, which the relation's scan
/// computes.
pub(crate) const EXPRESSION_COLUMN: u16 = 0x8000;
/// Expressions one query's classes hold.
pub(crate) const MAX_JOIN_EXPRESSIONS: usize = 16;

/// An equivalence class of columns compared by one btree and hash
/// operator family. A class holding a constant joins nothing: its members
/// are each restricted to the constant instead.
#[derive(Clone, Copy, Debug)]
pub(crate) struct EquivalenceClass<'a> {
    pub(crate) members: &'a [Member],
    pub(crate) has_const: bool,
}

impl<'a> EquivalenceClass<'a> {
    pub(crate) fn members(&self) -> &'a [Member] {
        self.members
    }

    /// Its member count.
    pub(crate) fn count(&self) -> usize {
        self.members.len()
    }

    /// The member a sort of rows of `relids` sorts by
    /// (`prepare_sort_from_pathkeys`): the first column among the input's
    /// target list, else the first expression it can compute.
    pub(crate) fn sort_member(&self, relids: Relids) -> Option<usize> {
        let members = self.members();
        let within = |member: &Member| member.base_relids() & !relids == Relids::EMPTY;
        members
            .iter()
            .position(|member| within(member) && member.expression_id().is_none())
            .or_else(|| members.iter().position(within))
    }

    /// `ec_relids`.
    pub(crate) fn relids(&self) -> Relids {
        self.members()
            .iter()
            .fold(Relids::EMPTY, |relids, member| relids | member.relids())
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
    /// A constant (the `true` an outer join clause gave way to), which a
    /// right or full merge join evaluates as a join condition
    /// (`select_mergejoin_clauses`).
    pub(crate) constant: bool,
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
        self.pushed_down || self.required & !relids != Relids::EMPTY
    }

    /// A clause of some relation's `joininfo`: one no class absorbed,
    /// required at more than one relation or outer join.
    fn in_joininfo(&self) -> bool {
        !matches!(self.form, ClauseForm::Equality { .. }) && self.required.count_ones() > 1
    }

    /// A clause of the `joininfo` of base relation `relids`.
    pub(crate) fn in_joininfo_of(&self, relids: Relids) -> bool {
        self.in_joininfo() && self.required & relids != Relids::EMPTY
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
/// relations it joins, `items[first..first + count]` of the problems'
/// items, a lower problem's joined relations as one item.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Subproblem {
    pub(crate) first: u16,
    pub(crate) count: u16,
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
    /// The relations a `LATERAL` subquery reads (`lateral_relids`, which
    /// are its `direct_lateral_relids`), empty for a table.
    pub(crate) lateral: Relids,
    /// The outer columns it reads (`lateral_vars`), which key a `Memoize`.
    pub(crate) lateral_vars: &'p [Member],
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
    pub(crate) geqo: GeqoSettings,
}

/// An ordered list of clause numbers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct ClauseList {
    ids: [ClauseId; MAX_JOIN_CLAUSES],
    len: u8,
}

impl ClauseList {
    pub(crate) const EMPTY: Self = Self {
        ids: [0; MAX_JOIN_CLAUSES],
        len: 0,
    };

    pub(crate) fn as_slice(&self) -> &[ClauseId] {
        &self.ids[..usize::from(self.len)]
    }

    fn push(&mut self, id: ClauseId) -> Option<()> {
        *self.ids.get_mut(usize::from(self.len))? = id;
        self.len += 1;
        Some(())
    }

    fn contains(&self, id: ClauseId) -> bool {
        self.as_slice().contains(&id)
    }

    /// Appends clause `id`, or `None` when the list is full.
    pub(crate) fn push_id(&mut self, id: ClauseId) -> Option<()> {
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
        /// The compared members, `JoinPlan::member_keys`' entry.
        keys: u32,
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
        /// The cache keys, `JoinPlan::member_keys`' entry.
        keys: u32,
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
        /// The inner relation is unique for the join's clauses.
        inner_unique: bool,
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
    /// The next relation of its hash chain ([`NO_REL`] ends it).
    next: u32,
}

/// No relation: the end of a hash chain.
const NO_REL: u32 = u32::MAX;
/// Hash chains of the relations one search builds (`join_rel_hash`).
const REL_BUCKETS: usize = 4096;

fn rel_bucket(relids: Relids) -> usize {
    ((relids & BASE_RELIDS).hash() % REL_BUCKETS as u64) as usize
}

/// The chosen plan: every path the search built, and the cheapest of the
/// relation joining them all.
pub(crate) struct JoinPlan<'a> {
    pub(crate) paths: &'a [Path],
    /// The members of each Unique and Memoize path.
    pub(crate) member_keys: &'a [[Member; MAX_KEYS]],
    pub(crate) clauses: &'a [Clause],
    pub(crate) classes: &'a [EquivalenceClass<'a>],
    /// The join's chosen path, below [`Self::upper`].
    pub(crate) top: u32,
    pub(crate) upper: UpperPlan,
    pub(crate) settings: JoinSettings,
    /// Each relation built, by its base relations, with the pair that
    /// built it.
    pub(crate) firsts: &'a [(Relids, (Relids, Relids))],
    pub(crate) needed: &'a [NeededColumn],
    /// The relations of removed joins, which the plan binds to NULLs.
    pub(crate) removed: Relids,
}

impl JoinPlan<'_> {
    pub(crate) fn path(&self, id: u32) -> &Path {
        &self.paths[id as usize]
    }

    /// The `count` members of a Unique or Memoize path's `keys` entry.
    pub(crate) fn keys(&self, keys: u32, count: u8) -> &[Member] {
        &self.member_keys[keys as usize][..usize::from(count)]
    }

    /// The columns of `relids`' target list in its order: each relation's
    /// needed columns that a relation outside `relids` or the output reads,
    /// a join relation's as its building pair's outer then inner lists.
    pub(crate) fn target_list(&self, relids: Relids, visit: &mut dyn FnMut(&NeededColumn)) {
        self.target_list_of(relids, relids, visit);
    }

    fn target_list_of(&self, part: Relids, above: Relids, visit: &mut dyn FnMut(&NeededColumn)) {
        let base = part & BASE_RELIDS;
        let (_, (outer, inner)) = *self
            .firsts
            .iter()
            .find(|(relids, _)| *relids == base)
            .expect("a relation of the plan");
        if inner == Relids::EMPTY {
            for column in self.needed {
                if part.contains(u32::from(column.rel))
                    && (column.output || !(column.needed_by & !above).is_empty())
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
    /// The query's window clauses, evaluated between the join and the
    /// sort.
    pub(crate) windows: WindowsProblem,
    /// The per-row cost of the target list the top window computes.
    pub(crate) windows_output_cost: f64,
}

impl UpperProblem {
    /// `query_pathkeys`: the first window's, else a grouping's, else
    /// [`Self::sort`].
    pub(crate) fn query_keys(&self) -> Keys {
        match (self.windows.as_slice().first(), self.group) {
            (Some(window), _) => window.keys,
            (None, Some(group)) if !group.distinct && !group.keys.as_slice().is_empty() => {
                group.keys
            }
            _ => self.sort,
        }
    }
}

/// The most window clauses a modeled query evaluates.
pub(crate) const MAX_WINDOWS: usize = 8;

/// The window clauses above the join, in the order PostgreSQL evaluates
/// them (`select_active_windows`).
#[derive(Clone, Copy, Debug)]
pub(crate) struct WindowsProblem {
    pub(crate) clauses: [WindowProblem; MAX_WINDOWS],
    pub(crate) count: u8,
    /// The width of the joined rows the bottom window reads
    /// (`make_window_input_target`).
    pub(crate) input_width: f64,
}

impl WindowsProblem {
    pub(crate) const NONE: Self = Self {
        clauses: [WindowProblem::NONE; MAX_WINDOWS],
        count: 0,
        input_width: 0.0,
    };

    pub(crate) fn as_slice(&self) -> &[WindowProblem] {
        &self.clauses[..usize::from(self.count)]
    }
}

/// How far a window clause's frame reaches past the current row, which
/// sets the rows its first output row reads
/// (`get_windowclause_startup_tuples`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum FrameEnd {
    UnboundedFollowing,
    /// `CURRENT ROW` in `ROWS` mode (`rows`), or in `RANGE` or `GROUPS`
    /// mode.
    CurrentRow {
        rows: bool,
    },
    OffsetPreceding,
    /// `n FOLLOWING`: the offset when it is an integer constant (a NULL's
    /// is one), else `None`.
    OffsetFollowing {
        rows: bool,
        offset: Option<f64>,
    },
}

/// One window clause (`create_one_window_path`, `cost_windowagg`).
#[derive(Clone, Copy, Debug)]
pub(crate) struct WindowProblem {
    /// `make_pathkeys_for_window`: its partition, then order pathkeys.
    pub(crate) keys: Keys,
    /// Its `PARTITION BY` and `ORDER BY` columns, as
    /// `estimate_num_groups` reads them.
    pub(crate) partition: [Member; MAX_KEYS],
    pub(crate) partition_count: u8,
    pub(crate) order: [Member; MAX_KEYS],
    pub(crate) order_count: u8,
    pub(crate) frame_end: FrameEnd,
    /// Its window functions' startup cost and cost per row.
    pub(crate) startup_cost: f64,
    pub(crate) per_row: f64,
    /// The width its functions' results add to the rows (for a window
    /// below the top one).
    pub(crate) added_width: f64,
}

impl WindowProblem {
    pub(crate) const NONE: Self = Self {
        keys: Keys::NONE,
        partition: [Member::plain(0, 0); MAX_KEYS],
        partition_count: 0,
        order: [Member::plain(0, 0); MAX_KEYS],
        order_count: 0,
        frame_end: FrameEnd::UnboundedFollowing,
        startup_cost: 0.0,
        per_row: 0.0,
        added_width: 0.0,
    };
}

/// A window clause's nodes as the plan chose them: the sort of its input
/// (startup and total cost, presorted keys, width), and its `WindowAgg`'s
/// startup and total cost and width.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct WindowStep {
    pub(crate) sort: Option<(f64, f64, u8)>,
    pub(crate) sort_width: f64,
    pub(crate) startup: f64,
    pub(crate) total: f64,
    pub(crate) width: f64,
}

/// A `GROUP BY` of plain columns, or the one group of a query's plain
/// aggregates, above the join (`create_grouping_paths`).
#[derive(Clone, Copy, Debug)]
pub(crate) struct GroupProblem {
    /// `group_pathkeys`, in `processed_groupClause` order, then the keys
    /// of the ordered aggregates it presorts for
    /// (`adjust_group_pathkeys_for_groupagg`); none for plain aggregation
    /// (`AGG_PLAIN`) of no presorted aggregate.
    pub(crate) keys: Keys,
    /// The grouping's own keys among [`Self::keys`]
    /// (`num_groupby_pathkeys`).
    pub(crate) columns: u8,
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
    /// The window clauses between the join and the sort, bottom first.
    pub(crate) windows: [WindowStep; MAX_WINDOWS],
}

/// The join problem.
pub(crate) struct JoinProblem<'p> {
    pub(crate) rels: &'p [BaseRel<'p>],
    pub(crate) classes: &'p [EquivalenceClass<'p>],
    /// The outer, semi and anti joins (`join_info_list`).
    pub(crate) specials: &'p [SpecialJoin],
    /// The join problems, lower ones first; the last joins them all.
    pub(crate) subproblems: &'p [Subproblem],
    /// The problems' items.
    pub(crate) subproblem_items: &'p [Relids],
    /// The query's join clauses in the order PostgreSQL distributes them,
    /// equalities of class members and other clauses above one relation
    /// included, then the clauses its base relations' index paths derived
    /// (`source` `None`).
    pub(crate) clauses: &'p [Clause],
    pub(crate) needed: &'p [NeededColumn],
    /// Each expression member's evaluation cost per row
    /// (`cost_qual_eval` of `em_expr`), by [`Member::expression_id`].
    pub(crate) expression_costs: &'p [f64],
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
    paths: ArenaChunks<'a, Path>,
    /// Each `Material` path built, by the path it reads.
    materials: ArenaList<'a, (u32, u32)>,
    /// The members of Unique and Memoize paths, kept apart so a path stays
    /// small.
    member_keys: ArenaList<'a, [Member; MAX_KEYS]>,
    /// The relations built (`join_rel_list`), base ones first.
    rels: ArenaChunks<'a, Rel>,
    /// The newest relation of each hash chain of [`Self::rels`].
    rel_buckets: &'a mut [u32],
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
    buckets: ArenaList<'a, [Option<(f64, f64)>; 2]>,
    /// `all_baserels`.
    all_baserels: Relids,
    /// `hasLateralRTEs`: a base relation is a `LATERAL` subquery.
    has_lateral: bool,
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
    // One relation is planned for the windows or ordered aggregates above
    // it.
    let least = if problem.upper.windows.count > 0
        || problem
            .upper
            .group
            .is_some_and(|group| group.aggregates.ordered)
    {
        1
    } else {
        2
    };
    if !(least..=MAX_JOIN_RELS).contains(&n) {
        return Ok(Err("a join of more relations than the join search holds"));
    }
    if problem.clauses.len() > MAX_CLAUSES {
        return Err(too_many_clauses());
    }
    let mut search = Search {
        problem,
        specials: &[],
        statistics,
        arena,
        clauses: ArenaList::new(arena),
        paths: ArenaChunks::new(arena, SEARCH_CHUNK),
        materials: ArenaList::new(arena),
        member_keys: ArenaList::new(arena),
        rels: ArenaChunks::new(arena, SEARCH_CHUNK),
        rel_buckets: arena_slice(arena, REL_BUCKETS, |_| NO_REL)?,
        levels: core::array::from_fn(|_| ArenaList::new(arena)),
        outside: None,
        memoize_rescans: ArenaList::new(arena),
        param_rows: ArenaList::new(arena),
        buckets: ArenaList::new(arena),
        all_baserels: Relids::below(n as u32) & !problem.removed,
        has_lateral: problem.rels.iter().any(|rel| rel.lateral != Relids::EMPTY),
        consider_startup: problem
            .upper
            .limit
            .is_some_and(|(count, _)| count.is_some()),
    };
    for clause in problem.clauses {
        search.clauses.push(*clause).map_err(|_| arena_full())?;
        search.buckets.push([None; 2]).map_err(|_| arena_full())?;
    }
    for (index, rel) in problem.rels.iter().enumerate() {
        let relids = Relids::of(index);
        // `distribute_restrictinfo_to_rels`: a clause required at several
        // relations or outer joins joins each of its base relations.
        let mut joininfo = ClauseList::EMPTY;
        for (id, clause) in problem.clauses.iter().enumerate() {
            if clause.in_joininfo() && clause.required.overlaps(relids) {
                joininfo.push(id as ClauseId).ok_or_else(too_many_clauses)?;
            }
        }
        search.insert_rel(Rel {
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
            first: (relids, Relids::EMPTY),
            unique: None,
            next: NO_REL,
        })?;
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
        search.set_cheapest(relids)?;
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
            param: Relids::EMPTY,
        })?;
        let mut paths = [0; MAX_REL_PATHS];
        paths[0] = id;
        search.insert_rel(Rel {
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
            first: (relids, Relids::EMPTY),
            unique: None,
            next: NO_REL,
        })?;
    }
    let geqo = problem.settings.geqo;
    for subproblem in problem.subproblems.iter().filter(|_| !problem.dummy) {
        let first = usize::from(subproblem.first);
        let items = &problem.subproblem_items[first..first + usize::from(subproblem.count)];
        if items.len() < 2 {
            continue;
        }
        search.levels = core::array::from_fn(|_| ArenaList::new(arena));
        for &item in items {
            // A lower problem's relation, with the outer joins it performs.
            let relids = search.rel(item).relids;
            search.levels[1].push(relids).map_err(|_| arena_full())?;
        }
        // `make_rel_from_joinlist`: a genetic search from `geqo_threshold`
        // relations on, else the exhaustive one.
        if geqo.enabled && items.len() as i64 >= i64::from(geqo.threshold) {
            if search.geqo()?.is_none() {
                return Ok(Err(search.outside.expect("the shape outside the model")));
            }
            continue;
        }
        for level in 2..=items.len() {
            search.one_level(level)?;
            if let Some(outside) = search.outside {
                return Ok(Err(outside));
            }
            for at in 0..search.levels[level].len() {
                let relids = search.levels[level].borrowed_slice()[at];
                search.set_cheapest(relids)?;
            }
        }
        if search.levels[items.len()].len() != 1 {
            return Ok(Err("a join search that builds no join"));
        }
    }
    let Some(top) = search.find_rel(search.all_baserels) else {
        return Ok(Err("a join the search does not reach"));
    };
    let top = search.rels.get(top);
    let (top, upper) = search.upper_paths(&top)?;
    let mut chosen = ChosenPaths {
        paths: ArenaList::new(arena),
        member_keys: ArenaList::new(arena),
        numbered: ArenaList::new(arena),
    };
    let top = chosen.take(&search.paths, search.member_keys.borrowed_slice(), top)?;
    let rels = &search.rels;
    Ok(Ok(JoinPlan {
        paths: chosen.paths.as_slice(),
        member_keys: chosen.member_keys.as_slice(),
        clauses: search.clauses.as_slice(),
        classes: {
            let classes = arena
                .alloc_slice_with(problem.classes.len(), |_| EquivalenceClass {
                    members: &[],
                    has_const: false,
                })
                .map_err(|_| arena_full())?;
            for (class, held) in classes.iter_mut().zip(problem.classes) {
                *class = EquivalenceClass {
                    members: arena
                        .alloc_slice_copy(held.members())
                        .map_err(|_| arena_full())?,
                    has_const: held.has_const,
                };
            }
            classes
        },
        top,
        upper,
        settings: problem.settings,
        firsts: arena
            .alloc_slice_with(rels.len(), |at| {
                let rel = rels.entry(at);
                (rel.relids & BASE_RELIDS, rel.first)
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

/// The chosen plan's paths alone, numbered from its leaves: the other paths
/// the search built are its working space, as `create_plan` copies only the
/// chosen paths into plan nodes, so what runs or explains the plan holds
/// state for its own nodes only.
struct ChosenPaths<'a> {
    paths: ArenaList<'a, Path>,
    member_keys: ArenaList<'a, [Member; MAX_KEYS]>,
    /// Each search path taken, with its number in [`Self::paths`]: a path
    /// two nodes read (a shared input) is taken once.
    numbered: ArenaList<'a, (u32, u32)>,
}

impl ChosenPaths<'_> {
    /// Takes path `id` of `paths` and the paths below it; returns its new
    /// number.
    fn take(
        &mut self,
        paths: &ArenaChunks<'_, Path>,
        member_keys: &[[Member; MAX_KEYS]],
        id: u32,
    ) -> Result<u32, SqlError> {
        if let Some(&(_, number)) = self
            .numbered
            .borrowed_slice()
            .iter()
            .find(|(old, _)| *old == id)
        {
            return Ok(number);
        }
        let mut path = paths.get(id as usize);
        let keys_of = |this: &mut Self, keys: &mut u32| -> Result<(), SqlError> {
            let number = this.member_keys.len() as u32;
            this.member_keys
                .push(member_keys[*keys as usize])
                .map_err(|_| arena_full())?;
            *keys = number;
            Ok(())
        };
        match &mut path.kind {
            PathKind::Dummy | PathKind::Scan { .. } => {}
            PathKind::Material { input } => *input = self.take(paths, member_keys, *input)?,
            PathKind::Unique { input, keys, .. } | PathKind::Memoize { input, keys, .. } => {
                *input = self.take(paths, member_keys, *input)?;
                keys_of(self, keys)?;
            }
            PathKind::NestLoop { outer, inner, .. }
            | PathKind::HashJoin { outer, inner, .. }
            | PathKind::MergeJoin { outer, inner, .. } => {
                *outer = self.take(paths, member_keys, *outer)?;
                *inner = self.take(paths, member_keys, *inner)?;
            }
        }
        let number = self.paths.len() as u32;
        self.paths.push(path).map_err(|_| arena_full())?;
        self.numbered.push((id, number)).map_err(|_| arena_full())?;
        Ok(number)
    }
}

fn arena_slice<T: Copy>(
    arena: &Arena,
    len: usize,
    fill: impl FnMut(usize) -> T,
) -> Result<&mut [T], SqlError> {
    arena.alloc_slice_with(len, fill).map_err(|_| arena_full())
}

/// The lengths [`Search::save_joins`] records.
#[derive(Clone, Copy)]
struct SavedJoins {
    rels: usize,
    paths: usize,
    materials: usize,
    member_keys: usize,
    memoize_rescans: usize,
    param_rows: usize,
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
    /// Keeps a Unique or Memoize path's members, returning their entry.
    fn push_member_keys(&mut self, keys: [Member; MAX_KEYS]) -> Result<u32, SqlError> {
        let id = self.member_keys.len() as u32;
        self.member_keys.push(keys).map_err(|_| arena_full())?;
        Ok(id)
    }

    fn push_path(&mut self, path: Path) -> Result<u32, SqlError> {
        let id = self.paths.len() as u32;
        self.paths.push(path).map_err(|_| arena_full())?;
        Ok(id)
    }

    fn path(&self, id: u32) -> Path {
        self.paths.get(id as usize)
    }

    fn clause(&self, id: ClauseId) -> Clause {
        self.clauses.borrowed_slice()[usize::from(id)]
    }

    /// Whether a clause list holds clause `id` or a copy of it: the copies
    /// of a commuting outer join's clause share its `rinfo_serial`.
    fn holds_serial(&self, list: &ClauseList, id: ClauseId) -> bool {
        let source = self.clause(id).source;
        list.as_slice()
            .iter()
            .any(|&held| held == id || (source.is_some() && self.clause(held).source == source))
    }

    /// `find_join_rel`: the relation of `relids`' base relations.
    fn find_rel(&self, relids: Relids) -> Option<usize> {
        let base = relids & BASE_RELIDS;
        let mut at = self.rel_buckets[rel_bucket(base)];
        while at != NO_REL {
            let rel = self.rels.entry(at as usize);
            if rel.relids & BASE_RELIDS == base {
                return Some(at as usize);
            }
            at = rel.next;
        }
        None
    }

    fn rel(&self, relids: Relids) -> Rel {
        let at = self.find_rel(relids).expect("the relation was built");
        self.rels.get(at)
    }

    /// Adds a relation not yet built (`add_join_rel`).
    fn insert_rel(&mut self, mut rel: Rel) -> Result<(), SqlError> {
        let bucket = rel_bucket(rel.relids);
        rel.next = self.rel_buckets[bucket];
        self.rel_buckets[bucket] = self.rels.len() as u32;
        self.rels.push(rel).map_err(|_| arena_full())
    }

    /// Replaces a built relation's state.
    fn store_rel(&mut self, rel: Rel) -> Result<(), SqlError> {
        let at = self.find_rel(rel.relids).expect("the relation was built");
        *self.rels.entry_mut(at) = rel;
        Ok(())
    }

    /// The relations and paths built so far, which [`Self::restore_joins`]
    /// returns to.
    fn save_joins(&self) -> SavedJoins {
        SavedJoins {
            rels: self.rels.len(),
            paths: self.paths.len(),
            materials: self.materials.len(),
            member_keys: self.member_keys.len(),
            memoize_rescans: self.memoize_rescans.len(),
            param_rows: self.param_rows.len(),
        }
    }

    /// Drops the relations and paths built since `saved`, as GEQO frees a
    /// tour's memory context. A kept relation's unique-ified path, which
    /// PostgreSQL builds in the planner's own context, is kept.
    fn restore_joins(&mut self, saved: SavedJoins) -> Result<(), SqlError> {
        for at in (saved.rels..self.rels.len()).rev() {
            let rel = self.rels.get(at);
            let bucket = rel_bucket(rel.relids);
            self.rel_buckets[bucket] = rel.next;
        }
        self.rels.truncate(saved.rels);
        let mut kept = [(0usize, None::<(Path, [Member; MAX_KEYS])>); 2 * MAX_JOIN_RELS];
        let mut kept_count = 0;
        for (at, rel) in self.rels.iter().enumerate() {
            if let Some(Some(id)) = rel.unique
                && id as usize >= saved.paths
            {
                let path = self.path(id);
                let keys = match path.kind {
                    PathKind::Unique { keys, .. } => {
                        self.member_keys.borrowed_slice()[keys as usize]
                    }
                    _ => unreachable!("a unique-ified path"),
                };
                *kept.get_mut(kept_count).ok_or_else(too_many_paths)? = (at, Some((path, keys)));
                kept_count += 1;
            }
        }
        self.paths.truncate(saved.paths);
        self.materials.truncate(saved.materials);
        self.member_keys.truncate(saved.member_keys);
        self.memoize_rescans.truncate(saved.memoize_rescans);
        self.param_rows.truncate(saved.param_rows);
        for &(at, held) in &kept[..kept_count] {
            let (mut path, members) = held.expect("a kept path");
            if let PathKind::Unique { keys, .. } = &mut path.kind {
                *keys = self.push_member_keys(members)?;
            }
            let id = self.push_path(path)?;
            self.rels.entry_mut(at).unique = Some(Some(id));
        }
        Ok(())
    }

    fn special(&self, id: u8) -> &SpecialJoin {
        &self.specials[usize::from(id)]
    }

    /// The sides of a mergeable clause: its left and right members and
    /// their classes (`left_ec`, `right_ec`).
    fn sides(&self, id: ClauseId) -> Option<(Member, Member, u8, u8)> {
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
    fn outer_is_left(&self, id: ClauseId, outer: Relids, inner: Relids) -> Option<bool> {
        let (left, right, ..) = self.sides(id)?;
        if left.relids() & !outer == Relids::EMPTY && right.relids() & !inner == Relids::EMPTY {
            Some(true)
        } else if right.relids() & !outer == Relids::EMPTY
            && left.relids() & !inner == Relids::EMPTY
        {
            Some(false)
        } else {
            None
        }
    }

    /// The class of a merge clause's outer side, and of its inner side.
    fn merge_classes(&self, id: ClauseId, outer: Relids, inner: Relids) -> (u8, u8) {
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
                && inner & !BASE_RELIDS == Relids::EMPTY
                && !self.problem.rels[inner.trailing_zeros() as usize]
                    .unique
                    .is_empty()
                && {
                    let joined = special.min_left | inner;
                    let mut restrict = ClauseList::EMPTY;
                    for (class_id, class) in self.problem.classes.iter().enumerate() {
                        if class.has_const || class.count() <= 1 {
                            continue;
                        }
                        let members = class.members();
                        let first = |side: Relids| {
                            members.iter().position(|member| {
                                member.relids() & !joined == Relids::EMPTY
                                    && member.relids() & !side == Relids::EMPTY
                            })
                        };
                        if let (Some(outer), Some(within)) = (first(special.min_left), first(inner))
                        {
                            let id =
                                self.class_clause(class_id as u8, outer as u8, within as u8)?;
                            restrict.push(id).ok_or_else(too_many_clauses)?;
                        }
                    }
                    for &id in self.rel(inner).joininfo.as_slice() {
                        restrict.push(id).ok_or_else(too_many_clauses)?;
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
            class.count() > 1
                && class.relids() & relids & BASE_RELIDS != Relids::EMPTY
                && class.relids() & !relids != Relids::EMPTY
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
            .any(|&id| self.clause(id).required & other != Relids::EMPTY)
        {
            return true;
        }
        self.has_eclass_joins(left)
            && self.has_eclass_joins(right)
            && self.problem.classes.iter().any(|class| {
                class.count() > 1
                    && class.relids() & left & BASE_RELIDS != Relids::EMPTY
                    && class.relids() & right & BASE_RELIDS != Relids::EMPTY
            })
    }

    /// `lateral_relids` of relation `relids`: its base relations' less
    /// its own (`min_join_parameterization`). Without placeholders a
    /// relation's `direct_lateral_relids` are these too.
    fn lateral_relids(&self, relids: Relids) -> Relids {
        if !self.has_lateral {
            return Relids::EMPTY;
        }
        (relids & BASE_RELIDS)
            .rels()
            .fold(Relids::EMPTY, |lateral, rel| {
                lateral | self.problem.rels[usize::from(rel)].lateral
            })
            & !relids
    }

    /// `lateral_referencers` of a base relation; a join relation has none.
    fn lateral_referencers(&self, relids: Relids) -> Relids {
        if !self.has_lateral || (relids & BASE_RELIDS).count_ones() != 1 {
            return Relids::EMPTY;
        }
        self.problem
            .rels
            .iter()
            .enumerate()
            .filter(|(_, rel)| rel.lateral.overlaps(relids))
            .fold(Relids::EMPTY, |referencers, (at, _)| {
                referencers | Relids::of(at)
            })
    }

    /// `has_join_restriction`.
    fn has_join_restriction(&self, relids: Relids) -> bool {
        if self.lateral_relids(relids) != Relids::EMPTY
            || self.lateral_referencers(relids) != Relids::EMPTY
        {
            return true;
        }
        self.specials.iter().any(|special| {
            special.kind != SpecialKind::Full
                && !(special.min_left & !relids == Relids::EMPTY
                    && special.min_right & !relids == Relids::EMPTY)
                && (special.min_left & relids != Relids::EMPTY
                    || special.min_right & relids != Relids::EMPTY)
        })
    }

    /// `have_join_order_restriction`.
    fn order_restriction(&mut self, left: Relids, right: Relids) -> Result<bool, SqlError> {
        // A direct lateral reference between the two joins them
        // regardless of outer joins.
        if self.lateral_relids(right).overlaps(left) || self.lateral_relids(left).overlaps(right) {
            return Ok(true);
        }
        let restricted = self.specials.iter().any(|special| {
            special.kind != SpecialKind::Full
                && ((special.min_left & !left == Relids::EMPTY
                    && special.min_right & !right == Relids::EMPTY)
                    || (special.min_left & !right == Relids::EMPTY
                        && special.min_right & !left == Relids::EMPTY)
                    || (special.min_right & left != Relids::EMPTY
                        && special.min_right & right != Relids::EMPTY)
                    || (special.min_left & left != Relids::EMPTY
                        && special.min_left & right != Relids::EMPTY))
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
            if other & relids != Relids::EMPTY || !self.relevant(relids, other) {
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
        let mut unique_ified = false;
        let mut must_be_left = false;
        let subset = |small: Relids, big: Relids| small & !big == Relids::EMPTY;
        for (id, special) in self.specials.iter().enumerate() {
            let id = id as u8;
            if special.min_right & joined == Relids::EMPTY || subset(joined, special.min_right) {
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
                unique_ified = true;
            } else if special.kind == SpecialKind::Semi
                && special.syn_right == rel1
                && self.unique_path(rel1, id)?.is_some()
            {
                if matched.is_some() {
                    return Ok(None);
                }
                matched = Some(id);
                reversed = true;
                unique_ified = true;
            } else {
                if special.min_right & rel1 != Relids::EMPTY
                    && special.min_right & rel2 != Relids::EMPTY
                {
                    continue;
                }
                if special.kind != SpecialKind::Left || joined & special.min_left != Relids::EMPTY {
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
        if self.has_lateral
            && !self.lateral_legal(rel1, rel2, joined, matched, reversed, unique_ified)
        {
            return Ok(None);
        }
        Ok(Some((matched, reversed)))
    }

    /// `join_is_legal`'s constraints of `LATERAL` references: the
    /// referencing side joins inside a nested loop, only to a relation it
    /// reads directly, and the join's remaining parameterization stays
    /// joinable past the outer joins' right sides.
    fn lateral_legal(
        &self,
        rel1: Relids,
        rel2: Relids,
        joined: Relids,
        matched: Option<u8>,
        reversed: bool,
        unique_ified: bool,
    ) -> bool {
        let forward = rel1.overlaps(self.lateral_relids(rel2));
        let backward = rel2.overlaps(self.lateral_relids(rel1));
        let not_nestloop = |reversed: bool| {
            matched.is_some_and(|id| {
                reversed || unique_ified || self.special(id).kind == SpecialKind::Full
            })
        };
        if forward && backward {
            return false;
        }
        if forward && not_nestloop(reversed) {
            return false;
        }
        if backward && not_nestloop(!reversed) {
            return false;
        }
        let lateral = self.lateral_relids(joined);
        if lateral == Relids::EMPTY {
            return true;
        }
        let mut plus_right = joined;
        loop {
            let mut more = false;
            for special in self.specials {
                if special.kind == SpecialKind::Full {
                    continue;
                }
                if special.min_left.overlaps(plus_right)
                    && special.min_right & !plus_right != Relids::EMPTY
                {
                    plus_right |= special.min_right;
                    more = true;
                }
            }
            if !more {
                break;
            }
        }
        !plus_right.overlaps(lateral)
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
        self.store_rel(rel)?;
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
        let held = self.push_member_keys(held)?;
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
        let rows = self.statistics.num_groups(keys, rel.rows)?;
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
        if special.oj == Relids::EMPTY {
            return input;
        }
        if special.kind != SpecialKind::Left {
            return input | special.oj;
        }
        if special.commute_below_l & !input != Relids::EMPTY {
            return input;
        }
        let mut relids = input | special.oj;
        if special.commute_above_l != Relids::EMPTY {
            let mut above = special.commute_above_l;
            for other in self.specials {
                if core::ptr::eq(other, special)
                    || other.oj == Relids::EMPTY
                    || other.kind != SpecialKind::Left
                {
                    continue;
                }
                if other.oj & above == Relids::EMPTY {
                    continue;
                }
                if other.oj & relids == Relids::EMPTY
                    && other.min_left & !relids == Relids::EMPTY
                    && other.min_right & !relids == Relids::EMPTY
                    && other.commute_below_l & !relids == Relids::EMPTY
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
                    if old & other == Relids::EMPTY
                        && (self.relevant(old, other) || self.order_restriction(old, other)?)
                    {
                        self.make_join_rel(old, other, level)?;
                    }
                }
            } else {
                for &other in singles {
                    if old & other == Relids::EMPTY {
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
                    if old & new == Relids::EMPTY
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
                    if old & other == Relids::EMPTY {
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
    /// The joined relation, `None` when the pair cannot join; a new one
    /// joins the search's `level` (none for GEQO's, `level` 0).
    fn make_join_rel(
        &mut self,
        rel1: Relids,
        rel2: Relids,
        level: usize,
    ) -> Result<Option<Relids>, SqlError> {
        let Some((special, reversed)) = self.join_is_legal(rel1, rel2, rel1 | rel2)? else {
            return Ok(None);
        };
        let joined = self.add_outer_joins(rel1 | rel2, special);
        let (rel1, rel2) = if reversed { (rel2, rel1) } else { (rel1, rel2) };
        let restrict = self.restrictlist(rel1, rel2, joined)?;
        match self.find_rel(joined).map(|at| self.rels.get(at)) {
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
                self.insert_rel(Rel {
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
                    next: NO_REL,
                })?;
                if level > 0 {
                    self.levels[level].push(joined).map_err(|_| arena_full())?;
                }
            }
            Some(rel) if rel.relids != joined => {
                self.outside = Some("a join relation formed with different outer joins");
                return Ok(None);
            }
            Some(_) => {}
        }
        if !self.is_dummy(joined) {
            self.populate_joinrel_with_paths(joined, rel1, rel2, special, restrict)?;
        }
        Ok(Some(joined))
    }

    /// `populate_joinrel_with_paths`.
    fn populate_joinrel_with_paths(
        &mut self,
        joined: Relids,
        rel1: Relids,
        rel2: Relids,
        special: Option<u8>,
        restrict: ClauseList,
    ) -> Result<(), SqlError> {
        let kind = special.map(|id| self.special(id).kind);
        // `populate_joinrel_with_paths`' proofs of an empty join: a dummy
        // input or the constant false condition of a class of two
        // constants, which is pushed down wherever it is evaluated.
        let false_relids = self.problem.false_relids;
        let constant_false = false_relids.count_ones() > 1
            && false_relids & !(joined & BASE_RELIDS) == Relids::EMPTY
            && false_relids & !rel1 != Relids::EMPTY
            && false_relids & !rel2 != Relids::EMPTY;
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
                if info.min_left & !rel1 == Relids::EMPTY && info.min_right & !rel2 == Relids::EMPTY
                {
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
            param: Relids::EMPTY,
        })?;
        rel.rows = 0.0;
        rel.paths[0] = id;
        rel.count = 1;
        rel.cheapest_total = id;
        rel.cheapest_startup = id;
        rel.parameterized[0] = id;
        rel.parameterized_count = 1;
        self.store_rel(rel)
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
                if clause.required & !joined != Relids::EMPTY {
                    continue;
                }
                // A copy of a commuting outer join's clause for another
                // order of the joins, which this pair's inputs must not
                // already have performed.
                if clause.clone != CloneKind::None
                    && (clause.required & !both != Relids::EMPTY
                        || clause.incompatible & both != Relids::EMPTY)
                {
                    continue;
                }
                if !list.contains(id) {
                    list.push(id).ok_or_else(too_many_clauses)?;
                }
            }
        }
        for (class_id, class) in self.problem.classes.iter().enumerate() {
            if class.has_const || class.count() <= 1 {
                continue;
            }
            let members = class.members();
            let computable = |member: &&Member| member.relids() & !joined == Relids::EMPTY;
            let outer_member = members
                .iter()
                .filter(computable)
                .position(|member| member.relids() & !outer == Relids::EMPTY);
            let inner_member = members
                .iter()
                .filter(computable)
                .position(|member| member.relids() & !inner == Relids::EMPTY);
            let (Some(_), Some(_)) = (outer_member, inner_member) else {
                continue;
            };
            let outer_member = members
                .iter()
                .position(|member| {
                    member.relids() & !joined == Relids::EMPTY
                        && member.relids() & !outer == Relids::EMPTY
                })
                .expect("found");
            let inner_member = members
                .iter()
                .position(|member| {
                    member.relids() & !joined == Relids::EMPTY
                        && member.relids() & !inner == Relids::EMPTY
                })
                .expect("found");
            let id = self.class_clause(class_id as u8, outer_member as u8, inner_member as u8)?;
            list.push(id).ok_or_else(too_many_clauses)?;
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
                if self.clause(id).required & !joined != Relids::EMPTY && !list.contains(id) {
                    list.push(id).ok_or_else(too_many_clauses)?;
                }
            }
        }
        Ok(list)
    }

    /// `create_join_clause`: the class's derived clause equating the two
    /// members in either orientation, else a new one oriented outer member
    /// first.
    fn class_clause(&mut self, class: u8, outer: u8, inner: u8) -> Result<ClauseId, SqlError> {
        // The query's own equalities are not searched: PostgreSQL derives
        // a clause for each split, oriented as the split that first needed
        // it, and reuses it in either orientation.
        let found = self.clauses.borrowed_slice().iter().position(|clause| {
            clause.source.is_none()
                && matches!(clause.form, ClauseForm::Equality { class: c, left, right }
                    if c == class && ((left, right) == (outer, inner) || (left, right) == (inner, outer)))
        });
        if let Some(id) = found {
            return ClauseId::try_from(id).map_err(|_| too_many_clauses());
        }
        let members = self.problem.classes[usize::from(class)].members();
        let relids = members[usize::from(outer)].relids() | members[usize::from(inner)].relids();
        let cost = self.problem.settings.costs.cpu_operator
            + self.member_cost(members[usize::from(outer)])
            + self.member_cost(members[usize::from(inner)]);
        let id = self.clauses.len();
        if id >= MAX_CLAUSES {
            return Err(too_many_clauses());
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
                incompatible: Relids::EMPTY,
                cost,
                selectivity: None,
                outer_selectivity: None,
                outer_relids: Relids::EMPTY,
                operands: None,
                other: OtherShape::Plain,
                constant: false,
                source: None,
            })
            .map_err(|_| arena_full())?;
        self.buckets.push([None; 2]).map_err(|_| arena_full())?;
        Ok(id as ClauseId)
    }

    /// `clause_selectivity` of a join clause, cached on the clause as
    /// PostgreSQL caches it on its `RestrictInfo`: one estimate for inner
    /// joins and one, whichever join computed it first, for the others.
    fn selectivity(&mut self, id: ClauseId, mode: Selectivity) -> Result<f64, SqlError> {
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
                let (outer, inner) =
                    if left.relids() & !special.syn_right & BASE_RELIDS == Relids::EMPTY {
                        (right, left)
                    } else {
                        (left, right)
                    };
                let Some(inner_rel) = self.find_rel(special.min_right).map(|at| self.rels.get(at))
                else {
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
                        if left.relids() & syn_right & BASE_RELIDS == Relids::EMPTY {
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

    fn set_clause(&mut self, id: ClauseId, clause: Clause) -> Result<(), SqlError> {
        self.clauses.as_mut_slice().map_err(|_| arena_full())?[usize::from(id)] = clause;
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
                relids & Relids::of(column.rel) != Relids::EMPTY
                    && (column.output || column.needed_by & !relids != Relids::EMPTY)
            })
            .map(|column| column.width)
            .sum()
    }

    /// `set_cheapest`.
    fn set_cheapest(&mut self, relids: Relids) -> Result<(), SqlError> {
        let mut rel = self.rel(relids);
        let mut total: Option<u32> = None;
        let mut startup: Option<u32> = None;
        let mut best_param: Option<u32> = None;
        let mut parameterized = [0u32; MAX_REL_PATHS];
        let mut parameterized_count = 1usize;
        for &id in &rel.paths[..usize::from(rel.count)] {
            let path = self.path(id);
            if path.param != Relids::EMPTY {
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
                        } else if path.param & !held.param == Relids::EMPTY {
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
        self.store_rel(rel)
    }

    /// `add_path` without parallel paths into `relids`' relation.
    fn add_path(&mut self, relids: Relids, new: Path) -> Result<(), SqlError> {
        let mut rel = self.rel(relids);
        let id = self.paths.len() as u32;
        let paths = &self.paths;
        let (count, accepted) = add_to_pathlist(
            &mut rel.paths,
            usize::from(rel.count),
            (id, new),
            &|id| paths.get(id as usize),
            self.consider_startup,
        )?;
        if accepted {
            self.push_path(new)?;
        }
        rel.count = count as u8;
        self.store_rel(rel)
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
        // The paths `create_ordered_paths` reads: the joined relation's
        // under its target list, or the window relation's.
        let mut inputs = [(
            0u32,
            self.path(rel.cheapest_total),
            [WindowStep::default(); MAX_WINDOWS],
        ); MAX_REL_PATHS];
        let (input_count, cheapest) = if upper.windows.count > 0 {
            // The joined rows carry the windows' input target list.
            let mut candidates = [(0u32, self.path(rel.cheapest_total)); MAX_REL_PATHS];
            for (at, &id) in rel.paths[..usize::from(rel.count)].iter().enumerate() {
                let mut path = self.path(id);
                path.width = upper.windows.input_width;
                path.total += upper.target_cost * path.rows;
                candidates[at] = (id, path);
            }
            self.window_paths(
                &candidates[..usize::from(rel.count)],
                rel.cheapest_total,
                &mut inputs,
            )?
        } else {
            let mut cheapest = 0;
            for (at, &id) in rel.paths[..usize::from(rel.count)].iter().enumerate() {
                let mut input = self.path(id);
                input.total += upper.target_cost * input.rows;
                inputs[at] = (id, input, [WindowStep::default(); MAX_WINDOWS]);
                if id == rel.cheapest_total {
                    cheapest = at;
                }
            }
            (usize::from(rel.count), cheapest)
        };
        // Each upper path, the join path below it and its sort and limit.
        let mut store = [inputs[cheapest].1; MAX_UPPER];
        let mut below = [(inputs[cheapest].0, None::<(f64, f64, u8)>, cheapest); MAX_UPPER];
        let mut stored = 0usize;
        let mut ordered = [0u32; MAX_REL_PATHS];
        let mut ordered_count = 0usize;
        for (at, &(id, input, _)) in inputs[..input_count].iter().enumerate() {
            let mut path = input;
            let mut sorted = None;
            let presorted = sort.common_prefix(&input.keys);
            if sort_len > 0 && presorted < sort_len {
                let incremental = presorted > 0 && settings.incremental_sort;
                if at != cheapest && !incremental {
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
                below[stored] = (id, None, at);
                stored += 1;
                continue;
            }
            store[stored] = path;
            below[stored] = (id, sorted, at);
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
                return Err(too_many_paths());
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
        let (top, sort, input) = below[chosen as usize];
        let joined = self.path(top);
        Ok((
            top,
            UpperPlan {
                sort,
                limit: limits[chosen as usize],
                top_total: joined.total + upper.target_cost * joined.rows,
                group: None,
                windows: inputs[input].2,
            },
        ))
    }

    /// `create_window_paths`: a window path over the joined relation's
    /// cheapest path and over each path sorted by a prefix of the first
    /// window's pathkeys, each window clause sorting its input unless it is
    /// already sorted (incrementally when it is by a prefix of its keys),
    /// kept as `add_path` keeps them. `candidates` are the input relation's
    /// paths under the windows' input target list, each tagged. Fills `out`
    /// with each kept path, its input's tag and its windows' nodes, and
    /// returns their count and the cheapest's position (`set_cheapest`).
    fn window_paths(
        &self,
        candidates: &[(u32, Path)],
        cheapest_tag: u32,
        out: &mut [(u32, Path, [WindowStep; MAX_WINDOWS]); MAX_REL_PATHS],
    ) -> Result<(usize, usize), SqlError> {
        let upper = self.problem.upper;
        let settings = self.problem.settings;
        let costs = settings.costs;
        let windows = upper.windows.as_slice();
        let mut built =
            [(0u32, candidates[0].1, [WindowStep::default(); MAX_WINDOWS]); MAX_REL_PATHS];
        let mut kept = [0u32; MAX_REL_PATHS];
        let mut kept_count = 0usize;
        let mut count = 0usize;
        for &(tag, input) in candidates {
            let mut path = input;
            let first = windows[0].keys;
            if tag != cheapest_tag
                && !first.contained_in(&path.keys)
                && first.common_prefix(&path.keys) == 0
            {
                continue;
            }
            let mut steps = [WindowStep::default(); MAX_WINDOWS];
            for (at, window) in windows.iter().enumerate() {
                let step = &mut steps[at];
                if !window.keys.contained_in(&path.keys) {
                    let presorted = window.keys.common_prefix(&path.keys);
                    let (startup, total) = if presorted == 0 || !settings.incremental_sort {
                        let (startup, run) =
                            super::paths::tuplesort_cost(path.rows, path.width, None, costs);
                        path.disabled += u32::from(!settings.sort);
                        (startup + path.total, startup + path.total + run)
                    } else {
                        self.incremental_sort_cost(
                            &path,
                            &window.keys,
                            presorted,
                            path.width,
                            None,
                        )?
                    };
                    step.sort = Some((
                        startup,
                        total,
                        if presorted == 0 || !settings.incremental_sort {
                            0
                        } else {
                            presorted as u8
                        },
                    ));
                    step.sort_width = path.width;
                    path.startup = startup;
                    path.total = total;
                    path.keys = window.keys;
                }
                // `cost_windowagg`.
                let rows = path.rows;
                let input_startup = path.startup + window.startup_cost;
                let mut total = path.total + window.per_row * rows;
                total += costs.cpu_operator
                    * f64::from(window.partition_count + window.order_count)
                    * rows;
                total += costs.cpu_tuple * rows;
                let mut startup = input_startup;
                let startup_tuples = self.window_startup_tuples(window, rows)?;
                if startup_tuples > 1.0 {
                    startup += (total - input_startup) / rows * (startup_tuples - 1.0);
                }
                // The top window computes the query's target list.
                if at + 1 == windows.len() {
                    total += upper.windows_output_cost * rows;
                    path.width = upper.width;
                } else {
                    path.width = (path.width + window.added_width).round();
                }
                path.startup = startup;
                path.total = total;
                step.startup = startup;
                step.total = total;
                step.width = path.width;
            }
            built[count] = (tag, path, steps);
            let (held, accepted) = add_to_pathlist(
                &mut kept,
                kept_count,
                (count as u32, path),
                &|at| built[at as usize].1,
                self.consider_startup,
            )?;
            kept_count = held;
            count += usize::from(accepted);
        }
        let mut cheapest = 0usize;
        for (at, &id) in kept[..kept_count].iter().enumerate() {
            out[at] = built[id as usize];
            let compared = compare_costs(&out[cheapest].1, &out[at].1, false);
            if at > 0
                && (compared > 0
                    || (compared == 0
                        && compare_keys(&out[cheapest].1.keys, &out[at].1.keys)
                            == KeysOrder::Better2))
            {
                cheapest = at;
            }
        }
        Ok((kept_count, cheapest))
    }

    /// `get_windowclause_startup_tuples`: the rows `window` reads from its
    /// `input_tuples` rows before it returns its first.
    fn window_startup_tuples(
        &self,
        window: &WindowProblem,
        input_tuples: f64,
    ) -> Result<f64, SqlError> {
        let partition_count = usize::from(window.partition_count);
        let order_count = usize::from(window.order_count);
        let partition_tuples = if partition_count > 0 {
            input_tuples
                / self
                    .statistics
                    .num_groups(&window.partition[..partition_count], input_tuples)?
        } else {
            input_tuples
        };
        let peer_tuples = if order_count > 0 {
            partition_tuples
                / self
                    .statistics
                    .num_groups(&window.order[..order_count], partition_tuples)?
        } else {
            1.0
        };
        let return_tuples = match window.frame_end {
            FrameEnd::UnboundedFollowing => partition_tuples,
            FrameEnd::CurrentRow { rows: true } | FrameEnd::OffsetPreceding => 1.0,
            FrameEnd::CurrentRow { rows: false } if order_count == 0 => partition_tuples,
            FrameEnd::CurrentRow { rows: false } => peer_tuples,
            FrameEnd::OffsetFollowing { rows, offset } => {
                // `DEFAULT_INEQ_SEL` of the partition's peer groups for an
                // offset that is no integer constant.
                let offset =
                    offset.unwrap_or(partition_tuples / peer_tuples * 0.333_333_333_333_333_3);
                if rows {
                    offset + 1.0
                } else {
                    peer_tuples * (offset + 1.0)
                }
            }
        };
        let return_tuples = if partition_count > 0 || order_count > 0 {
            (return_tuples + 1.0).min(partition_tuples)
        } else {
            return_tuples.min(partition_tuples)
        };
        Ok(clamp_row_est(return_tuples))
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
        let columns = usize::from(group.columns);
        let sort_len = group.keys.as_slice().len();
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
        // A sorted `GROUP BY` without aggregates is `create_group_path`'s
        // `Group`.
        let aggregate = |strategy: AggStrategy, input: AggInput| {
            let costed = if strategy == AggStrategy::Sorted && !group.aggregates.aggregated() {
                super::aggregate::cost_group(columns, groups, input)
            } else {
                cost_agg(strategy, &group.aggregates, columns, groups, input)
            };
            let (startup, total, rows) = super::aggregate::with_having(costed, group.having);
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
            if input.param != Relids::EMPTY {
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
                let grouping = &group.keys.as_slice()[..columns];
                for &key in input.keys.as_slice() {
                    if reordered.as_slice().len() >= columns || !grouping.contains(&key) {
                        break;
                    }
                    let _ = reordered.push(key);
                }
                let matched = reordered.as_slice().len();
                for &key in grouping {
                    if !reordered.as_slice().contains(&key) {
                        let _ = reordered.push(key);
                    }
                }
                // The presorted aggregates' keys stay last.
                for &key in &group.keys.as_slice()[columns..] {
                    let _ = reordered.push(key);
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
                if presorted < sort_len {
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
                    return Err(too_many_paths());
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
                return Err(too_many_paths());
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
        // `create_window_paths` over the grouped paths, each window path
        // stored with its grouped path's plan.
        let mut window_steps = [[WindowStep::default(); MAX_WINDOWS]; MAX_GROUPED];
        let mut sort_width = group.output_width;
        if upper.windows.count > 0 {
            let mut candidates = [(0u32, store[cheapest_grouped as usize]); MAX_REL_PATHS];
            for (at, &id) in grouped[..grouped_count].iter().enumerate() {
                candidates[at] = (id, store[id as usize]);
            }
            let mut windowed = [(
                0u32,
                store[cheapest_grouped as usize],
                [WindowStep::default(); MAX_WINDOWS],
            ); MAX_REL_PATHS];
            let (count, cheapest) = self.window_paths(
                &candidates[..grouped_count],
                cheapest_grouped,
                &mut windowed,
            )?;
            grouped_count = 0;
            for (at, &(tag, path, steps)) in windowed[..count].iter().enumerate() {
                if stored == MAX_GROUPED {
                    return Err(too_many_paths());
                }
                store[stored] = path;
                plans[stored] = plans[tag as usize];
                window_steps[stored] = steps;
                grouped[grouped_count] = stored as u32;
                grouped_count += 1;
                if at == cheapest {
                    cheapest_grouped = stored as u32;
                }
                stored += 1;
            }
            sort_width = upper.width;
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
                    self.incremental_sort_cost(&input, &sort, presorted, sort_width, limit_tuples)?
                } else {
                    let (startup, run) =
                        super::paths::tuplesort_cost(input.rows, sort_width, limit_tuples, costs);
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
                return Err(too_many_paths());
            }
            store[stored] = path;
            plans[stored] = plans[at as usize];
            window_steps[stored] = window_steps[at as usize];
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
                return Err(too_many_paths());
            }
            store[stored] = path;
            plans[stored] = plans[at as usize];
            window_steps[stored] = window_steps[at as usize];
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
                windows: window_steps[chosen as usize],
            },
        ))
    }

    /// `eclass_useful_for_merging`.
    fn useful_for_merging(&self, class: u8, relids: Relids) -> bool {
        let class = &self.problem.classes[usize::from(class)];
        if class.has_const || class.count() <= 1 || class.relids() & !relids == Relids::EMPTY {
            return false;
        }
        class
            .members()
            .iter()
            .any(|member| member.relids() & relids == Relids::EMPTY)
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
        let useful = useful.max(self.problem.upper.query_keys().common_prefix(outer));
        outer.truncated(useful)
    }

    /// `right_merge_direction`: a pathkey is useful for merging in the
    /// direction the query orders its class, ascending when it does not.
    fn right_merge_direction(&self, key: u8) -> bool {
        let wanted = self
            .problem
            .upper
            .query_keys()
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
                nonmergeable |= !self.clause(id).constant;
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
                special.min_left & !outer == Relids::EMPTY
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
        let mut param_source = Relids::EMPTY;
        for other in self.specials {
            if joined & other.min_right != Relids::EMPTY && joined & other.min_left == Relids::EMPTY
            {
                param_source |= self.all_baserels & !other.min_right;
            }
            if other.kind == SpecialKind::Full
                && joined & other.min_left != Relids::EMPTY
                && joined & other.min_right == Relids::EMPTY
            {
                param_source |= self.all_baserels & !other.min_left;
            }
        }
        // A `LATERAL` subquery's paths are all parameterized by what it
        // reads: the join's remaining lateral references are sources too.
        param_source |= self.lateral_relids(joined);
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
                .filter(|member| member.relids() & joined == Relids::EMPTY)
                .count() as i32;
            classes[count] = class;
            count += 1;
        }
        let mut all = Keys::NONE;
        if count == 0 {
            return all;
        }
        let query = self.problem.upper.query_keys();
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
        if self.path(outer_path).param & inputs.inner != Relids::EMPTY
            || self.path(inner_path).param & inputs.outer != Relids::EMPTY
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
    fn merge_direction(&self, id: ClauseId, inputs: &JoinInputs, outer_keys: &Keys) -> u8 {
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
            .filter(|&path| self.path(path).param & inputs.outer == Relids::EMPTY);
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
            if self.path(held_outer).param & inputs.inner != Relids::EMPTY {
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
            let (referencing, referenced) =
                (Relids::of(key.referencing), Relids::of(key.referenced));
            let referenced_outer = if outer & referencing != Relids::EMPTY
                && inner & referenced != Relids::EMPTY
            {
                false
            } else if outer & referenced != Relids::EMPTY && inner & referencing != Relids::EMPTY {
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
        if restrict.len == 0 || inner.count_ones() != 1 || inner & !BASE_RELIDS != Relids::EMPTY {
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
            || inner_path.param == Relids::EMPTY
            || inner_path.param & !inputs.outer != Relids::EMPTY
        {
            return Ok(None);
        }
        if !inputs.inner_unique && matches!(inputs.jointype, JoinType::Semi | JoinType::Anti) {
            return Ok(None);
        }
        let enforced = self.enforced(inner);
        // A `LATERAL` subquery's outer columns key the cache too
        // (`innerrel->lateral_vars`).
        let lateral_vars = match inner_path.kind {
            PathKind::Scan { rel, .. } => self.problem.rels[usize::from(rel)].lateral_vars,
            _ => &[],
        };
        if (enforced.len == 0 && lateral_vars.is_empty())
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
            let key = if left.relids() & !inputs.outer == Relids::EMPTY
                && right.relids() & !inputs.inner == Relids::EMPTY
            {
                left
            } else if right.relids() & !inputs.outer == Relids::EMPTY
                && left.relids() & !inputs.inner == Relids::EMPTY
            {
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
        // `paraminfo_get_equal_hashops` keys lateral references in binary
        // mode: it cannot tell how the subquery reads them.
        for &key in lateral_vars {
            binary = true;
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
        let keys = self.push_member_keys(keys)?;
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

    /// A member's evaluation cost per row: none for a column.
    fn member_cost(&self, member: Member) -> f64 {
        member
            .expression_id()
            .map_or(0.0, |id| self.problem.expression_costs[id])
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
        if path.param == Relids::EMPTY {
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
                    relids & !available == Relids::EMPTY
                        && relids & current != Relids::EMPTY
                        && outer_relids & current == Relids::EMPTY
                })
            };
        let join_and_req = Some(joined | required);
        let outer_and_req = (outer_param != Relids::EMPTY).then_some(inputs.outer | outer_param);
        let inner_and_req = (inner_param != Relids::EMPTY).then_some(inputs.inner | inner_param);
        let mut moved = ClauseList::EMPTY;
        for &id in self.rel(joined).joininfo.as_slice() {
            let clause = self.clause(id);
            let (relids, outer_relids) = (clause.relids, clause.outer_relids);
            if movable(relids, outer_relids, joined, join_and_req)
                && !movable(relids, outer_relids, inputs.outer, outer_and_req)
                && !movable(relids, outer_relids, inputs.inner, inner_and_req)
            {
                moved.push(id).ok_or_else(too_many_clauses)?;
            }
        }
        let mut dropped = [0u8; MAX_JOIN_CLAUSES];
        let mut dropped_count = 0;
        let first = |members: &[Member], relids: Relids| {
            members
                .iter()
                .position(|member| member.relids() & !relids == Relids::EMPTY)
        };
        for class_id in 0..self.problem.classes.len() {
            let class = self.problem.classes[class_id];
            if class.has_const || class.count() <= 1 {
                continue;
            }
            let members = class.members();
            let (Some(outside), Some(within)) = (first(members, required), first(members, joined))
            else {
                continue;
            };
            let relids = members[outside].relids() | members[within].relids();
            if movable(relids, Relids::EMPTY, inputs.outer, outer_and_req) {
                continue;
            }
            if movable(relids, Relids::EMPTY, inputs.inner, inner_and_req) {
                if dropped_count < dropped.len() {
                    dropped[dropped_count] = class_id as u8;
                    dropped_count += 1;
                }
                continue;
            }
            let id = self.class_clause(class_id as u8, outside as u8, within as u8)?;
            moved.push(id).ok_or_else(too_many_clauses)?;
        }
        for &class_id in &dropped[..dropped_count] {
            let members = self.problem.classes[usize::from(class_id)].members();
            let (Some(outside), Some(outer)) =
                (first(members, required), first(members, inputs.outer))
            else {
                continue;
            };
            let relids = members[outside].relids() | members[outer].relids();
            if movable(relids, Relids::EMPTY, inputs.outer, outer_and_req) {
                continue;
            }
            let id = self.class_clause(class_id, outside as u8, outer as u8)?;
            moved.push(id).ok_or_else(too_many_clauses)?;
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

    /// `create_material_path` with `cost_material`: one path per input, as
    /// its costs and rows are the input's alone (PostgreSQL builds one per
    /// join pair and never frees it). A relation's path appears once in a
    /// plan, so no plan reads the shared path twice.
    fn material_path(&mut self, input: u32) -> Result<u32, SqlError> {
        if let Some(&(_, material)) = self
            .materials
            .borrowed_slice()
            .iter()
            .rev()
            .find(|(over, _)| *over == input)
        {
            return Ok(material);
        }
        let path = self.path(input);
        let settings = self.problem.settings;
        let (startup, total) =
            material_cost(path.startup, path.total, path.rows, path.width, settings);
        let material = self.push_path(Path {
            kind: PathKind::Material { input },
            startup,
            total,
            disabled: path.disabled + u32::from(!settings.material),
            ..path
        })?;
        self.materials
            .push((input, material))
            .map_err(|_| arena_full())?;
        Ok(material)
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
            if path.param != Relids::EMPTY {
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
            if oj != Relids::EMPTY && (inner_path.param | outer_path.param) & oj != Relids::EMPTY {
                return Ok(());
            }
        }
        // `calc_nestloop_required_outer`: what the join still reads from
        // other relations, which the search allows from its parameter
        // sources or where `allow_star_schema_join` lets it build a
        // parameterized join.
        let required = (outer_path.param | inner_path.param) & !inputs.outer;
        let mut moved = ClauseList::EMPTY;
        if required != Relids::EMPTY {
            let star = inner_path.param & inputs.outer != Relids::EMPTY
                && inner_path.param & !inputs.outer != Relids::EMPTY;
            if required & inputs.param_source == Relids::EMPTY && !star {
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
        let enforced = if inner_path.param & inputs.outer != Relids::EMPTY {
            self.enforced(inner)
        } else {
            ClauseList::EMPTY
        };
        for &id in inputs.restrict.as_slice() {
            if !self.holds_serial(&enforced, id) {
                clauses.push(id).ok_or_else(too_many_clauses)?;
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
        if required == Relids::EMPTY {
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
        if required == Relids::EMPTY {
            return Ok(Some((Relids::EMPTY, inputs.restrict)));
        }
        if required & inputs.param_source == Relids::EMPTY {
            return Ok(None);
        }
        let mut clauses = self.moved_join_clauses(
            joined,
            inputs,
            (outer_path.param, inner_path.param),
            required,
        )?;
        for &id in inputs.restrict.as_slice() {
            clauses.push(id).ok_or_else(too_many_clauses)?;
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
        if self.path(total_outer).param & inputs.inner != Relids::EMPTY
            || self.path(total_inner).param & inputs.outer != Relids::EMPTY
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
            if self.path(outer).param & inputs.inner != Relids::EMPTY {
                continue;
            }
            for &inner in &inner_rel.parameterized[..usize::from(inner_rel.parameterized_count)] {
                if self.path(inner).param & inputs.outer != Relids::EMPTY {
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
        id: ClauseId,
        inner: Member,
        left: bool,
        buckets: f64,
    ) -> Result<(f64, f64), SqlError> {
        let side = usize::from(!left);
        if let Some(held) = self.buckets.borrowed_slice()[usize::from(id)][side] {
            return Ok(held);
        }
        let estimate = self.statistics.bucket(inner, buckets)?;
        self.buckets.as_mut_slice().map_err(|_| arena_full())?[usize::from(id)][side] =
            Some(estimate);
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
        // Costing always reserves the skew table (`useskew`).
        let sizing = choose_hash_table_size(inner_path.rows, inner_path.width, true, settings);
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
                    inner_unique: inputs.inner_unique,
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

pub(crate) fn too_many_clauses() -> SqlError {
    crate::sql_err!(
        crate::sql::eval::sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "join search capacity exceeded: a join or relation holds at most {} join clauses and a query {}",
        MAX_JOIN_CLAUSES,
        MAX_CLAUSES
    )
}

fn too_many_paths() -> SqlError {
    crate::sql_err!(
        crate::sql::eval::sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "join search capacity exceeded: a relation holds at most {} paths",
        MAX_REL_PATHS
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
    useskew: bool,
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
    let mut skew_mcvs = if useskew {
        ((table_bytes / per_mcv).trunc() * 2.0 / 100.0).trunc()
    } else {
        0.0
    };
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
    let consider_startup = consider_startup && new.param == Relids::EMPTY;
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
            let new_keys = if new.param != Relids::EMPTY {
                Keys::NONE
            } else {
                new.keys
            };
            let old_keys = if old.param != Relids::EMPTY {
                Keys::NONE
            } else {
                old.keys
            };
            let keys = compare_keys(&new_keys, &old_keys);
            // `bms_subset_compare` of the parameterizations.
            let outer = match (
                new.param & !old.param == Relids::EMPTY,
                old.param & !new.param == Relids::EMPTY,
            ) {
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
            return Err(too_many_paths());
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
            geqo: GeqoSettings {
                enabled: true,
                threshold: 12,
                effort: 5,
                pool_size: 0,
                generations: 0,
                selection_bias: 2.0,
                seed: 0.0,
            },
        }
    }

    #[test]
    fn hash_table_sizes_follow_exec_choose_hash_table_size() {
        // 1000 rows of 12 bytes fit work_mem's doubled hash memory.
        assert_eq!(
            choose_hash_table_size(1000.0, 12.0, true, settings()),
            HashSizing {
                buckets: 1024,
                batches: 1,
                skew_mcvs: 1271,
                space_allowed: 8.0 * 1024.0 * 1024.0,
            }
        );
        assert_eq!(
            choose_hash_table_size(5000.0, 12.0, true, settings()).buckets,
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
            param: Relids::EMPTY,
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
        let class = EquivalenceClass {
            members: &[Member::plain(0, 0), Member::plain(1, 0)],
            has_const: false,
        };
        let problem = JoinProblem {
            rels: &[
                BaseRel {
                    rows: 1000.0,
                    width: 12.0,
                    paths: &[seq(1000.0, 16.0)],
                    unique: &[],
                    fixed: 0,
                    lateral: Relids::EMPTY,
                    lateral_vars: &[],
                },
                BaseRel {
                    rows: 2000.0,
                    width: 8.0,
                    paths: &[seq(2000.0, 29.0)],
                    unique: &[],
                    fixed: 0,
                    lateral: Relids::EMPTY,
                    lateral_vars: &[],
                },
            ],
            classes: &[class],
            specials: &[],
            subproblems: &[Subproblem { first: 0, count: 2 }],
            subproblem_items: &[Relids::of(0u8), Relids::of(1u8)],
            clauses: &[Clause {
                form: ClauseForm::Equality {
                    class: 0,
                    left: 0,
                    right: 1,
                },
                relids: Relids::from_mask(3),
                required: Relids::from_mask(3),
                pushed_down: true,
                clone: CloneKind::None,
                incompatible: Relids::EMPTY,
                cost: 0.0025,
                selectivity: None,
                outer_selectivity: None,
                outer_relids: Relids::EMPTY,
                operands: None,
                other: OtherShape::Plain,
                constant: false,
                source: Some(0),
            }],
            needed: &[
                NeededColumn {
                    rel: 0,
                    column: 0,
                    width: 12.0,
                    needed_by: Relids::from_mask(2),
                    output: true,
                },
                NeededColumn {
                    rel: 1,
                    column: 0,
                    width: 8.0,
                    needed_by: Relids::from_mask(1),
                    output: true,
                },
            ],
            expression_costs: &[],
            settings: settings(),
            foreign_keys: &[],
            removed: Relids::EMPTY,
            upper: UpperProblem {
                sort: Keys::NONE,
                limit: None,
                width: 20.0,
                target_cost: 0.0,
                group: None,
                windows: WindowsProblem::NONE,
                windows_output_cost: 0.0,
            },
            dummy: false,
            false_relids: Relids::EMPTY,
        };
        let plan = search(&problem, &Uniform, &arena).unwrap().unwrap();
        let top = plan.path(plan.top);
        let PathKind::HashJoin { outer, inner, .. } = top.kind else {
            panic!("{top:?}");
        };
        assert_eq!(plan.path(outer).relids, Relids::from_mask(2));
        assert_eq!(plan.path(inner).relids, Relids::from_mask(1));
        assert_eq!((top.startup, top.total, top.rows), (28.5, 85.0, 2000.0));
    }

    /// [`Uniform`] whose `Memoize` keys repeat: ten distinct values.
    struct TenKeys;

    impl JoinStatistics for TenKeys {
        fn equality(&self, left: Member, right: Member) -> Result<f64, SqlError> {
            Uniform.equality(left, right)
        }
        fn semi(&self, left: Member, right: Member, rows: f64) -> Result<f64, SqlError> {
            Uniform.semi(left, right, rows)
        }
        fn bucket(&self, member: Member, buckets: f64) -> Result<(f64, f64), SqlError> {
            Uniform.bucket(member, buckets)
        }
        fn merge_range(
            &self,
            left: Member,
            right: Member,
            left_descending: bool,
            right_descending: bool,
        ) -> Result<Option<[f64; 4]>, SqlError> {
            Uniform.merge_range(left, right, left_descending, right_descending)
        }
        fn groups(&self, _: &[Member], calls: f64) -> Result<f64, SqlError> {
            Ok(calls.min(10.0))
        }
        fn num_groups(&self, members: &[Member], rows: f64) -> Result<f64, SqlError> {
            Uniform.num_groups(members, rows)
        }
    }

    #[test]
    fn lateral_subquery_joins_inside_a_memoized_nested_loop() {
        // lo(a, x) 1000 rows, a with ten values, and `LATERAL (SELECT
        // count(*) FROM li WHERE li.b = lo.a) s`, whose plan costs
        // 85.625..85.635 for its one row, as PostgreSQL plans
        // `SELECT * FROM lo, LATERAL (...) s`: Nested Loop
        // (cost=85.64..981.12) over a Memoize keyed by lo.a.
        let mut budget = crate::mem::budget::Budget::new(1 << 21);
        let arena = Arena::new(&mut budget, "join search", 1 << 20).unwrap();
        let lateral = BasePath {
            startup: 85.625,
            total: 85.635,
            rows: 1.0,
            param: Relids::of(0u8),
            ..seq(1.0, 85.635)
        };
        let base = |rows, width, paths, lateral, lateral_vars| BaseRel {
            rows,
            width,
            paths,
            unique: &[],
            fixed: 0,
            lateral,
            lateral_vars,
        };
        let needed = |rel, column, width, needed_by| NeededColumn {
            rel,
            column,
            width,
            needed_by,
            output: true,
        };
        let (outer_paths, lateral_paths) = ([seq(1000.0, 15.0)], [lateral]);
        let lateral_vars = [Member::plain(0, 0)];
        let rels = [
            base(1000.0, 8.0, &outer_paths, Relids::EMPTY, &[]),
            base(1.0, 8.0, &lateral_paths, Relids::of(0u8), &lateral_vars),
        ];
        let mut problem = JoinProblem {
            rels: &rels,
            classes: &[],
            specials: &[],
            subproblems: &[Subproblem { first: 0, count: 2 }],
            subproblem_items: &[Relids::of(0u8), Relids::of(1u8)],
            clauses: &[],
            needed: &[
                needed(0, 0, 4.0, Relids::of(1u8)),
                needed(0, 1, 4.0, Relids::EMPTY),
                needed(1, 0, 8.0, Relids::EMPTY),
            ],
            expression_costs: &[],
            settings: settings(),
            foreign_keys: &[],
            removed: Relids::EMPTY,
            upper: UpperProblem {
                sort: Keys::NONE,
                limit: None,
                width: 16.0,
                target_cost: 0.0,
                group: None,
                windows: WindowsProblem::NONE,
                windows_output_cost: 0.0,
            },
            dummy: false,
            false_relids: Relids::EMPTY,
        };
        let plan = search(&problem, &TenKeys, &arena).unwrap().unwrap();
        let top = plan.path(plan.top);
        let PathKind::NestLoop { outer, inner, .. } = top.kind else {
            panic!("{top:?}");
        };
        assert_eq!(plan.path(outer).relids, Relids::of(0u8));
        let PathKind::Memoize {
            key_count, binary, ..
        } = plan.path(inner).kind
        else {
            panic!("{:?}", plan.path(inner));
        };
        assert_eq!((key_count, binary), (1, true));
        assert_eq!(
            format!("{:.2}..{:.2}", top.startup, top.total),
            "85.64..981.12"
        );
        assert_eq!(top.rows, 1000.0);
        // Without a cache the subquery runs once per outer row.
        problem.settings.memoize = false;
        let plan = search(&problem, &TenKeys, &arena).unwrap().unwrap();
        let top = plan.path(plan.top);
        let PathKind::NestLoop { inner, .. } = top.kind else {
            panic!("{top:?}");
        };
        assert!(matches!(
            plan.path(inner).kind,
            PathKind::Scan { rel: 1, .. }
        ));
        assert_eq!(
            format!("{:.2}..{:.2}", top.startup, top.total),
            "85.62..85660.00"
        );
    }
}
