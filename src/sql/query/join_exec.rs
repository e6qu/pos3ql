//! Runs the join plan PostgreSQL's join search chose
//! ([`super::index_paths::join_query`]) through its pull nodes ([`pull`]),
//! so its rows come back in PostgreSQL's order: a nested loop reads its
//! inner input again (or its `Materialize` copy) for each outer row; a hash
//! join builds its table on the first outer row, stops at an empty one, and
//! returns each outer row's matches newest first, as `ExecHashJoin` walks a
//! bucket's chain; a merge join reads its sorted outer input first (its
//! inner one only when the outer has rows) and returns each outer row's run
//! of inner matches in inner order. An outer join returns each unmatched
//! row of a side it keeps with NULLs for the other, as each executor node
//! does. Sorts order as PostgreSQL's `pg_qsort` does. This module holds
//! what every node reads: the plan's row layout, its comparisons, hashes
//! and orders.
//! <https://github.com/postgres/postgres/blob/REL_18_6/src/backend/executor/nodeHashjoin.c>
//! <https://github.com/postgres/postgres/blob/REL_18_6/src/backend/executor/nodeMergejoin.c>

use core::cmp::Ordering;

use crate::mem::arena::Arena;
use crate::sql::ast::Collation;
use crate::sql::eval::{ColumnLookup, EvalHooks, SqlError};
use crate::sql::types::Datum;
use crate::storage::{Storage, StoredForm};

use super::index_paths::join_query::{ModeledJoin, PlannedJoin};
use super::index_paths::joins::{
    ClauseList, JoinType, KEY_CLASS, KEY_DESCENDING, KEY_NULLS_FIRST, Keys, Member, PathKind,
    RELATION_NUMBERS, Relids,
};
use super::scan::{JoinRow, JoinRowValues, PaxReadDemand};
use super::{QueryScope, arena_full};

/// A flat outer row: its values, each table's offset in them and each
/// value's stored form.
type OuterValues<'a> = (&'a [Datum<'a>], &'a [usize], &'a [StoredForm]);

mod pull;
pub(crate) use pull::{ModeledPull, pulls};

/// The plan node numbers of a join's nodes under `EXPLAIN ANALYZE`, past
/// the numbers the block's own nodes use.
pub(crate) fn node_number(path: u32) -> Option<u16> {
    u16::try_from(path).ok()?.checked_add(1000)
}

/// The block a base relation's scan counts in: its own, so each scan's
/// nodes are those of a query of that one relation.
pub(crate) fn scan_segment(rel: usize) -> super::instrument::Segment {
    super::instrument::Segment::Derived(u16::try_from(rel).unwrap_or(u16::MAX))
}

fn node(role: impl FnOnce(u16) -> super::instrument::Role, path: u32) -> super::instrument::Node {
    node_number(path).map_or(super::instrument::Node::NONE, |number| {
        super::instrument::node(role(number))
    })
}

/// One base row: its values and row identity.
#[derive(Clone, Copy)]
struct BaseRow<'a> {
    values: &'a [Datum<'a>],
    /// The stored forms of the relation's columns; empty when every one is
    /// as `heap_form_tuple` forms it.
    forms: &'a [StoredForm],
    rowid: Option<u64>,
}

/// A skew bucket (`HashSkewBucket`): its slot in the skew array.
#[derive(Clone, Copy)]
struct SkewSlot {
    position: u32,
    hash: u32,
    removed: bool,
}

/// The skew buckets `ExecHashBuildSkewHash` fills.
trait SkewBuckets {
    /// Enables `length` skew slots, charging `space` bytes.
    fn enable_skew(&mut self, length: u32, space: usize);
    /// The hash of the skew slot at `position`, when one is taken there.
    fn skew_slot(&self, position: u32) -> Option<u32>;
    /// Takes a skew slot (`HashSkewBucket`), charging its space.
    fn take_skew_slot(&mut self, slot: SkewSlot) -> Result<(), SqlError>;
}

/// Whether [`datum_hash`] hashes values of `ctype`: the types whose default
/// hash operator class it computes.
pub(crate) fn datum_type_hashable(ctype: crate::sql::types::ColType) -> bool {
    use crate::sql::types::ColType;
    matches!(
        ctype,
        ColType::Int2
            | ColType::Int4
            | ColType::Int8
            | ColType::Oid
            | ColType::Cid
            | ColType::Date
            | ColType::Bool
            | ColType::Char
            | ColType::Text
            | ColType::Varchar
            | ColType::Name
            | ColType::Bpchar
            | ColType::Bytea
            | ColType::Uuid
            | ColType::Numeric
            | ColType::Float4
            | ColType::Float8
            | ColType::Timestamp
            | ColType::Timestamptz
            | ColType::Time
            | ColType::Timetz
            | ColType::Interval
            | ColType::PgLsn
            | ColType::Inet
            | ColType::Cidr
            | ColType::Macaddr
            | ColType::Macaddr8
    )
}

/// The value hashes `ExecHashGetHashValue` and `TupleHashTableHash`
/// combine: each type's default hash support function, `hashtext` and
/// `hashbpchar` under a deterministic collation.
/// https://github.com/postgres/postgres/blob/REL_18_6/src/backend/access/hash/hashfunc.c
pub(crate) fn datum_hash(value: &Datum<'_>) -> Option<u32> {
    use crate::sql::identity::{hash_bytea, hash_int64_input, hash_uint32};
    let bytes = |value: &[u8]| hash_bytea(value) as u32;
    Some(match value {
        Datum::Int2(value) => hash_uint32(*value as i32 as u32),
        Datum::Int4(value) | Datum::Date(value) => hash_uint32(*value as u32),
        Datum::Oid(value) | Datum::Cid(value) => hash_uint32(*value),
        Datum::Int8(value)
        | Datum::Timestamp(value)
        | Datum::Timestamptz(value)
        | Datum::Time(value) => hash_uint32(hash_int64_input(*value)),
        Datum::PgLsn(value) | Datum::Xid8(value) => hash_uint32(hash_int64_input(*value as i64)),
        Datum::Bool(value) => hash_uint32(u32::from(*value)),
        Datum::Char(value) => hash_uint32((*value as i8 as i32) as u32),
        Datum::Text(value) => bytes(value.as_bytes()),
        // `hashbpchar` hashes the value without its trailing blanks.
        Datum::Bpchar(value) => bytes(value.trim_end_matches(' ').as_bytes()),
        Datum::Bytea(value) => bytes(value),
        Datum::Uuid(value) => bytes(value),
        Datum::Macaddr(value) => bytes(value),
        Datum::Macaddr8(value) => bytes(value),
        Datum::Numeric(value) => numeric_hash(value),
        // `hashfloat4` hashes its value as a `float8`.
        Datum::Float4(value) => float8_hash(f64::from(*value)),
        Datum::Float8(value) => float8_hash(*value),
        // `timetz_hash`: the time's `hashint8` mixed with its zone, which
        // PostgreSQL stores as seconds west of UTC.
        Datum::Timetz(micros, offset) => {
            hash_uint32(hash_int64_input(*micros)) ^ hash_uint32(offset.wrapping_neg() as u32)
        }
        // `interval_hash`: the low 64 bits of its 30-day-month span.
        Datum::Interval(interval) => {
            let span = i128::from(interval.micros)
                + i128::from(interval.days) * 86_400_000_000
                + i128::from(interval.months) * 30 * 86_400_000_000;
            hash_uint32(hash_int64_input(span as i64))
        }
        // `hashinet`: the family (`PGSQL_AF_INET` or `PGSQL_AF_INET6`), the
        // mask length and the address bytes.
        Datum::Inet(address) | Datum::Cidr(address) => {
            let mut held = [0u8; 18];
            held[0] = if address.family() == 4 { 2 } else { 3 };
            held[1] = address.bits();
            let length = address.addr_len();
            held[2..2 + length].copy_from_slice(&address.addr()[..length]);
            bytes(&held[..2 + length])
        }
        _ => return None,
    })
}

/// [`datum_hash`] of a grouping or distinct key, which also hashes an enum
/// value (`hashenum`: `hash_uint32` of its label's `pg_enum` OID), an array
/// (`hash_array`), a record (`hash_record`), a range (`hash_range`), a
/// multirange (`hash_multirange`) and a `jsonb` document (`jsonb_hash`).
/// `arena` holds a parsed number or decoded string while it is hashed.
pub(crate) fn key_hash(
    value: &Datum<'_>,
    storage: &Storage,
    txid: u32,
    arena: &crate::mem::arena::Arena,
) -> Result<Option<u32>, SqlError> {
    // `hash_array` and `hash_record` hash a null member as zero and combine
    // the members' hashes as `result * 31 + hash`.
    let combine = |result: u32, member: &Datum<'_>| -> Result<Option<u32>, SqlError> {
        let hashed = if matches!(member, Datum::Null) {
            0
        } else {
            match key_hash(member, storage, txid, arena)? {
                Some(hashed) => hashed,
                None => return Ok(None),
            }
        };
        Ok(Some(result.wrapping_mul(31).wrapping_add(hashed)))
    };
    Ok(match value {
        Datum::Enum { .. } => {
            let oid = crate::sql::enum_values::member_oid(value, storage, txid)?;
            Some(crate::sql::identity::hash_uint32(oid))
        }
        Datum::Array { element, raw } => {
            let mut result = 1u32;
            for member in crate::sql::array::structured_elements(raw, *element, arena) {
                match combine(result, &member?)? {
                    Some(next) => result = next,
                    None => return Ok(None),
                }
            }
            Some(result)
        }
        Datum::CompositeText {
            slot,
            physical_fields,
            text,
        } => {
            let decoded = crate::sql::exec::decode_stored_composite_text(
                *text,
                *slot,
                *physical_fields,
                storage,
                txid,
                arena,
            )?;
            return key_hash(&decoded, storage, txid, arena);
        }
        Datum::Record(fields) | Datum::Composite { fields, .. } => {
            let mut result = 0u32;
            for field in *fields {
                match combine(result, &field.value)? {
                    Some(next) => result = next,
                    None => return Ok(None),
                }
            }
            Some(result)
        }
        Datum::Range { text, kind } => {
            Some(crate::sql::range::hash_range(text, *kind, None, arena)? as u32)
        }
        Datum::Multirange { text, kind } => {
            Some(crate::sql::range::hash_multirange(text, *kind, None, arena)? as u32)
        }
        Datum::Json { text, jsonb: true } => {
            Some(crate::sql::json::compare::hash_jsonb(text, arena)?)
        }
        _ => datum_hash(value),
    })
}

/// Whether [`key_hash`] hashes a key of type `ctype`.
pub(crate) fn key_type_hashable(ctype: crate::sql::types::ColType) -> bool {
    use crate::sql::types::ColType;
    match ctype {
        ColType::Enum(_) | ColType::Jsonb => true,
        ColType::Array(element) => key_type_hashable(element.to_coltype()),
        ColType::Range(kind) | ColType::Multirange(kind) => key_type_hashable(kind.elem_type()),
        _ => datum_type_hashable(ctype),
    }
}

/// Whether PostgreSQL has no hash operator class for a key of type `ctype`,
/// so it always sorts to group it: `tsvector`, `tsquery`, `money`, bit
/// strings, an anonymous record (whose field types the type cache cannot
/// vouch for), and arrays, ranges and multiranges of such elements.
pub(crate) fn key_type_sorts_only(ctype: crate::sql::types::ColType) -> bool {
    use crate::sql::types::ColType;
    match ctype {
        ColType::TsVector
        | ColType::TsQuery
        | ColType::Money
        | ColType::Bit { .. }
        | ColType::Record => true,
        ColType::Array(element) => key_type_sorts_only(element.to_coltype()),
        ColType::Range(kind) | ColType::Multirange(kind) => key_type_sorts_only(kind.elem_type()),
        _ => false,
    }
}

/// Whether two values equal by their type's equality have the same stored
/// image (`datum_image_eq`).
fn same_image(left: &Datum<'_>, right: &Datum<'_>) -> bool {
    match (left, right) {
        (Datum::Numeric(one), Datum::Numeric(two)) => one.dscale == two.dscale,
        (Datum::Float4(one), Datum::Float4(two)) => one.to_bits() == two.to_bits(),
        (Datum::Float8(one), Datum::Float8(two)) => one.to_bits() == two.to_bits(),
        (Datum::Interval(one), Datum::Interval(two)) => {
            (one.months, one.days, one.micros) == (two.months, two.days, two.micros)
        }
        (Datum::Timetz(one, one_zone), Datum::Timetz(two, two_zone)) => {
            (one, one_zone) == (two, two_zone)
        }
        (Datum::Bpchar(one), Datum::Bpchar(two)) => one == two,
        _ => true,
    }
}

/// `hash_numeric`: the base-10000 digits between the first and last
/// nonzero ones, in native byte order, the weight mixed in by XOR; NaN and
/// the infinities hash to zero and zero to all ones.
pub(crate) fn numeric_hash(value: &crate::sql::numeric::Numeric<'_>) -> u32 {
    use crate::sql::numeric::Sign;
    if matches!(value.sign, Sign::NaN | Sign::PosInf | Sign::NegInf) {
        return 0;
    }
    let count = value.ndigits();
    let Some(first) = (0..count).find(|&at| value.digit(at) != 0) else {
        return u32::MAX;
    };
    let last = (0..count)
        .rev()
        .find(|&at| value.digit(at) != 0)
        .expect("a nonzero digit exists");
    let mut held = [0u8; 2 * crate::sql::numeric::MAX_NDIGITS];
    for (at, index) in (first..=last).enumerate() {
        held[2 * at..2 * at + 2].copy_from_slice(&value.digit(index).to_ne_bytes());
    }
    let weight = i32::from(value.weight) - first as i32;
    (crate::sql::identity::hash_bytea(&held[..2 * (last - first + 1)]) as u32) ^ weight as u32
}

/// `hashfloat8`: zero of either sign hashes to zero, and every NaN alike.
fn float8_hash(value: f64) -> u32 {
    if value == 0.0 {
        return 0;
    }
    let value = if value.is_nan() { f64::NAN } else { value };
    crate::sql::identity::hash_bytea(&value.to_ne_bytes()) as u32
}

/// What `MJEvalOuterValues` and `MJEvalInnerValues` make of a row.
#[derive(Clone, Copy, PartialEq, Eq)]
enum MergeEval {
    Matchable,
    NonMatchable,
    EndOfJoin,
}

/// What an index scan a merge join reads in part reads per row: an index
/// entry, and its row's heap page.
#[derive(Clone, Copy)]
struct LazyPages<'a> {
    model: super::instrument::PageModel,
    /// `(row identity, heap page)` sorted by identity.
    heap: &'a [(u64, u64)],
    /// An index-only scan's all-visible pages, which precede the others:
    /// a row on a later page is a heap fetch.
    visible: Option<u64>,
    /// `BUFFERS` counts the pages.
    buffers: bool,
}

/// Counts an execution of the index scan `lazy` a join read in part:
/// `fetched` rows, and by `pages` the index entry and heap page of each
/// fetched row of identity `rowids`.
fn count_read_in_part(
    lazy: super::instrument::Node,
    pages: Option<LazyPages<'_>>,
    fetched: usize,
    rowids: impl Iterator<Item = Option<u64>>,
) {
    let _scan = lazy.start();
    lazy.rows(fetched as u64);
    let Some(pages) = pages else {
        return;
    };
    if pages.buffers {
        lazy.page_model(pages.model);
    }
    for rowid in rowids {
        if pages.buffers {
            lazy.index_entry();
        }
        let Some(rowid) = rowid else {
            continue;
        };
        let Ok(at) = pages.heap.binary_search_by_key(&rowid, |&(id, _)| id) else {
            continue;
        };
        let page = pages.heap[at].1;
        match pages.visible {
            Some(visible) if page < visible => {}
            Some(_) => {
                lazy.heap_fetch();
                if pages.buffers {
                    lazy.heap_page(page);
                }
            }
            None if pages.buffers => lazy.heap_page(page),
            None => {}
        }
    }
}

/// Each key clause's (outer, inner) column; unused entries hold `usize::MAX`.
type KeySides = [((usize, usize), (usize, usize)); super::index_paths::joins::MAX_JOIN_CLAUSES];

/// The flat row layout of a join's relations, which every node of its plan
/// evaluates conditions over, made once per plan.
pub(super) struct JoinLayout<'a> {
    /// Each relation's values' start in a flat row, and a row of NULLs per
    /// relation, which an outer join binds to the side it null-extends.
    offsets: &'a [usize],
    width: usize,
    nulls: &'a [&'a [Datum<'a>]],
    /// The first error an expression member of several relations raised
    /// while the join compared rows by it; the query fails with it before
    /// another row leaves the join.
    failed: core::cell::Cell<Option<SqlError>>,
    /// The flat row an expression of several relations is evaluated over,
    /// allocated once, and its values' stored forms.
    scratch: core::cell::RefCell<&'a mut [Datum<'a>]>,
    form_scratch: core::cell::RefCell<&'a mut [StoredForm]>,
}

impl<'a> JoinLayout<'a> {
    /// The layout of `model`'s relations, resolved in `scope`.
    pub(super) fn new(
        model: &ModeledJoin<'a>,
        scope: &QueryScope<'a>,
        arena: &'a Arena,
    ) -> Result<Self, SqlError> {
        let n = scope.n;
        let offsets = arena
            .alloc_slice_with(n, |_| 0usize)
            .map_err(|_| arena_full())?;
        let mut width = 0;
        for (table, offset) in offsets.iter_mut().enumerate() {
            *offset = width;
            width += scope.stored_width(table);
        }
        // A row of NULLs per relation: its columns, then a NULL for each
        // expression member.
        let mut nulls = [&[][..]; RELATION_NUMBERS];
        for (table, held) in nulls.iter_mut().enumerate().take(n) {
            *held = arena
                .alloc_slice_with(scope.stored_width(table) + model.expressions.len(), |_| {
                    Datum::Null
                })
                .map_err(|_| arena_full())?;
        }
        Ok(Self {
            offsets,
            width,
            nulls: arena
                .alloc_slice_copy(&nulls[..n])
                .map_err(|_| arena_full())?,
            failed: core::cell::Cell::new(None),
            scratch: core::cell::RefCell::new(
                arena
                    .alloc_slice_with(width, |_| Datum::Null)
                    .map_err(|_| arena_full())?,
            ),
            form_scratch: core::cell::RefCell::new(
                arena
                    .alloc_slice_with(width, |_| StoredForm::Inline)
                    .map_err(|_| arena_full())?,
            ),
        })
    }
}

/// What every node of a join's plan reads to compare and bind rows: the
/// plan, its relations' layout and the statement's evaluation context.
#[derive(Clone, Copy)]
pub(super) struct JoinContext<'r, 'a> {
    storage: &'r Storage,
    scope: &'r QueryScope<'a>,
    txid: u32,
    model: &'r ModeledJoin<'a>,
    arena: &'a Arena,
    params: &'r [Datum<'a>],
    hooks: &'r EvalHooks<'r, 'a>,
    offsets: &'a [usize],
    width: usize,
    nulls: &'a [&'a [Datum<'a>]],
    failed: &'r core::cell::Cell<Option<SqlError>>,
    scratch: &'r core::cell::RefCell<&'a mut [Datum<'a>]>,
    form_scratch: &'r core::cell::RefCell<&'a mut [StoredForm]>,
}

impl<'r, 'a> JoinContext<'r, 'a> {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        storage: &'r Storage,
        scope: &'r QueryScope<'a>,
        txid: u32,
        model: &'r ModeledJoin<'a>,
        arena: &'a Arena,
        params: &'r [Datum<'a>],
        hooks: &'r EvalHooks<'r, 'a>,
        layout: &'r JoinLayout<'a>,
    ) -> Self {
        Self {
            storage,
            scope,
            txid,
            model,
            arena,
            params,
            hooks,
            offsets: layout.offsets,
            width: layout.width,
            nulls: layout.nulls,
            failed: &layout.failed,
            scratch: &layout.scratch,
            form_scratch: &layout.form_scratch,
        }
    }

    fn bind(&self, bound: &mut [Option<BaseRow<'a>>], row: &[Option<BaseRow<'a>>], relids: Relids) {
        for table in 0..self.scope.n {
            if relids.contains(table as u32) {
                bound[table] = row[table];
            }
        }
    }

    /// Records `error` unless an earlier one was: the read fails with the
    /// first at its next check.
    fn fail(&self, error: SqlError) {
        if let Some(first) = self.failed.take() {
            self.failed.set(Some(first));
        } else {
            self.failed.set(Some(error));
        }
    }

    /// Fails with the first error an expression of several relations
    /// raised.
    fn check(&self) -> Result<(), SqlError> {
        match self.failed.take() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// A member's value in the bound row: a column, an expression member
    /// its scan computed after the columns, or an expression of several
    /// relations evaluated over their bound rows.
    fn value(&self, row: &[Option<BaseRow<'a>>], (table, column): (usize, usize)) -> Datum<'a> {
        if let Some(id) = Member::plain(0, column as u16).expression_id()
            && self.model.expressions[id].reads != Relids::EMPTY
        {
            return match self.evaluate(row, self.model.expressions[id].expression) {
                Ok(value) => value,
                Err(error) => {
                    if let Some(first) = self.failed.take() {
                        self.failed.set(Some(first));
                    } else {
                        self.failed.set(Some(error));
                    }
                    Datum::Null
                }
            };
        }
        row[table].map_or(Datum::Null, |row| {
            row.values[self.value_index(row.values.len(), column)]
        })
    }

    /// Where a member's value sits in a base row of `len` values.
    fn value_index(&self, len: usize, column: usize) -> usize {
        match Member::plain(0, column as u16).expression_id() {
            Some(id) => len - self.model.expressions.len() + id,
            None => column,
        }
    }

    /// The columns of a base row, without its expression members' values.
    fn columns_of<'v>(&self, values: &'v [Datum<'a>]) -> &'v [Datum<'a>] {
        &values[..values.len() - self.model.expressions.len()]
    }

    fn collation(&self, (table, column): (usize, usize)) -> Collation {
        if let Some(id) = Member::plain(0, column as u16).expression_id() {
            return self.model.expressions[id].collation;
        }
        self.scope
            .defs
            .stored(table)
            .expect("a plain table")
            .columns()[column]
            .collation
    }

    /// Each clause's (outer, inner) columns.
    fn sides(&self, clauses: &ClauseList, outer: Relids) -> KeySides {
        let mut sides =
            [((usize::MAX, 0), (usize::MAX, 0)); super::index_paths::joins::MAX_JOIN_CLAUSES];
        for (at, &id) in clauses.as_slice().iter().enumerate() {
            let (left, right) = self
                .model
                .clause_columns(&self.model.plan.clauses[usize::from(id)])
                .expect("a hash or merge clause equates columns");
            sides[at] = if outer.contains(left.0 as u32) {
                (left, right)
            } else {
                (right, left)
            };
        }
        sides
    }

    /// A join's clauses as its plan evaluates them
    /// (`extract_actual_join_clauses`, `order_qual_clauses`): an outer
    /// join's matching conditions apart from the filters pushed down to it,
    /// each list cheapest first, without the clauses its hash or merge keys
    /// apply.
    fn split(
        &self,
        clauses: &ClauseList,
        relids: Relids,
        jointype: JoinType,
        keyed: &ClauseList,
    ) -> (ClauseList, ClauseList) {
        crate::sql::query::index_paths::joins::split_join_clauses(
            self.model.plan.clauses,
            clauses,
            relids,
            jointype,
            keyed,
        )
    }

    /// Binds a row of NULLs to the relations of `relids`.
    fn null_extend(&self, bound: &mut [Option<BaseRow<'a>>], relids: Relids) {
        for (table, (slot, nulls)) in bound.iter_mut().zip(self.nulls).enumerate() {
            if relids.contains(table as u32) {
                *slot = Some(BaseRow {
                    values: nulls,
                    forms: &[],
                    rowid: None,
                });
            }
        }
    }

    /// The stored forms of the bound rows of `relids` (all when `None`) at
    /// their places in a flat row, into `forms`; every other entry `Inline`.
    fn flat_forms(
        &self,
        bound: &[Option<BaseRow<'a>>],
        relids: Option<Relids>,
        forms: &mut [StoredForm],
    ) {
        forms.fill(StoredForm::Inline);
        for (table, row) in bound.iter().enumerate().take(self.scope.n) {
            if let Some(row) = row
                && relids.is_none_or(|relids| relids.contains(table as u32))
            {
                let start = self.offsets[table];
                forms[start..start + row.forms.len()].copy_from_slice(row.forms);
            }
        }
    }

    /// `expression` over the bound rows, its value kept in the arena.
    fn evaluate(
        &self,
        bound: &[Option<BaseRow<'a>>],
        expression: &crate::sql::ast::Expr<'a>,
    ) -> Result<Datum<'a>, SqlError> {
        let mut scratch = self.scratch.borrow_mut();
        let values: &mut [Datum<'a>] = &mut scratch;
        values.fill(Datum::Null);
        for (table, row) in bound.iter().enumerate().take(self.scope.n) {
            if let Some(row) = row {
                let start = self.offsets[table];
                let columns = self.columns_of(row.values);
                values[start..start + columns.len()].copy_from_slice(columns);
            }
        }
        let mut forms = self.form_scratch.borrow_mut();
        self.flat_forms(bound, None, &mut forms);
        let row = JoinRow {
            scope: self.scope,
            values: JoinRowValues::Flat {
                values,
                offsets: self.offsets,
                forms: Some(&forms),
            },
            rowids: &[],
            visible: self.scope.n,
        };
        crate::sql::eval::eval_full(expression, self.arena, self.params, &row, self.hooks)
    }

    /// Whether a condition no equality models holds for the bound rows.
    fn condition(
        &self,
        bound: &[Option<BaseRow<'a>>],
        expression: &crate::sql::ast::Expr<'a>,
    ) -> Result<bool, SqlError> {
        let mark = self.arena.mark();
        let values = self
            .arena
            .alloc_slice_with(self.width, |_| Datum::Null)
            .map_err(|_| arena_full())?;
        for (table, row) in bound.iter().enumerate().take(self.scope.n) {
            if let Some(row) = row {
                let start = self.offsets[table];
                let columns = self.columns_of(row.values);
                values[start..start + columns.len()].copy_from_slice(columns);
            }
        }
        let forms = self
            .arena
            .alloc_slice_with(self.width, |_| StoredForm::Inline)
            .map_err(|_| arena_full())?;
        self.flat_forms(bound, None, forms);
        let row = JoinRow {
            scope: self.scope,
            values: JoinRowValues::Flat {
                values,
                offsets: self.offsets,
                forms: Some(forms),
            },
            rowids: &[],
            visible: self.scope.n,
        };
        let value =
            crate::sql::eval::eval_full(expression, self.arena, self.params, &row, self.hooks)?;
        let holds = matches!(value, Datum::Bool(true));
        // SAFETY: nothing the evaluation allocated outlives it: the result
        // was read as a flag.
        unsafe { self.arena.rewind_to(mark) };
        Ok(holds)
    }

    fn keys(keys: &KeySides) -> impl Iterator<Item = &((usize, usize), (usize, usize))> {
        keys.iter().take_while(|key| key.0.0 != usize::MAX)
    }

    /// Whether every clause of `clauses` holds for the bound rows.
    fn equal(&self, bound: &[Option<BaseRow<'a>>], clauses: &ClauseList) -> Result<bool, SqlError> {
        for &id in clauses.as_slice() {
            let clause = &self.model.plan.clauses[usize::from(id)];
            let Some((left, right)) = self.model.clause_columns(clause) else {
                let expression = self.model.condition(clause).expect("a written condition");
                if !self.condition(bound, expression)? {
                    return Ok(false);
                }
                continue;
            };
            let (one, two) = (self.value(bound, left), self.value(bound, right));
            if one.is_null()
                || two.is_null()
                || crate::sql::eval::compare_datums_collated(
                    self.storage,
                    self.txid,
                    self.collation(left),
                    &one,
                    &two,
                )? != Ordering::Equal
            {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn has_null(&self, row: &[Option<BaseRow<'a>>], keys: &KeySides, outer: bool) -> bool {
        Self::keys(keys).any(|(o, i)| self.value(row, if outer { *o } else { *i }).is_null())
    }

    /// The bound outer row's keys against `inner`'s keys.
    fn compare_sides(
        &self,
        bound: &[Option<BaseRow<'a>>],
        inner: &[Option<BaseRow<'a>>],
        keys: &KeySides,
    ) -> Result<Ordering, SqlError> {
        for &(o, i) in Self::keys(keys) {
            let order = crate::sql::eval::compare_datums_collated(
                self.storage,
                self.txid,
                self.collation(o),
                &self.value(bound, o),
                &self.value(inner, i),
            )?;
            if order != Ordering::Equal {
                return Ok(order);
            }
        }
        Ok(Ordering::Equal)
    }

    /// `create_hashjoin_plan`'s skew table: the join hashes one key whose
    /// outer side is a column.
    fn skewed(&self, keys: &KeySides) -> bool {
        JoinContext::keys(keys).count() == 1
            && Member::plain(0, keys[0].0.1 as u16)
                .expression_id()
                .is_none()
    }

    /// The combined hash of a row's join keys (`ExecHashGetHashValue`):
    /// `None` when a key is NULL.
    fn key_hash(
        &self,
        row: &[Option<BaseRow<'a>>],
        keys: &KeySides,
        outer: bool,
        keep_nulls: bool,
    ) -> Result<Option<u32>, SqlError> {
        let mut hash = 0u32;
        for (at, &(o, i)) in JoinContext::keys(keys).enumerate() {
            let value = self.value(row, if outer { o } else { i });
            if value.is_null() {
                // A join returning the side's unmatched rows hashes a NULL
                // key as nothing (`EEOP_HASHDATUM_FIRST`, `NEXT32`).
                if !keep_nulls {
                    return Ok(None);
                }
                if at != 0 {
                    hash = hash.rotate_left(1);
                }
                continue;
            }
            let one = datum_hash(&value).ok_or_else(|| {
                crate::sql_err!(
                    crate::sql::eval::sqlstate::FEATURE_NOT_SUPPORTED,
                    "a hash join over keys of this type is not supported"
                )
            })?;
            hash = if at == 0 {
                one
            } else {
                hash.rotate_left(1) ^ one
            };
        }
        Ok(Some(hash))
    }

    fn build_skew(
        &self,
        table: &mut dyn SkewBuckets,
        (rel, column): (usize, usize),
        sizing: super::index_paths::joins::HashSizing,
    ) -> Result<(), SqlError> {
        let wanted = sizing.skew_mcvs as usize;
        if wanted == 0 {
            return Ok(());
        }
        let slot = self.scope.slots[rel];
        let definition = self.scope.defs.stored(rel).expect("a plain table");
        let statistics = self.storage.table_statistics(slot, self.txid);
        let Some(distribution) = definition
            .attributes
            .number(column)
            .and_then(|attribute| statistics.index_distribution(0, attribute))
        else {
            return Ok(());
        };
        let ctype = definition.columns()[column].ctype;
        let count = wanted.min(distribution.mcv_len());
        if count == 0 {
            return Ok(());
        }
        let fraction: f64 = (0..count)
            .map(|index| f64::from(distribution.mcv_frequency(index)))
            .sum();
        // `SKEW_MIN_OUTER_FRACTION`.
        if fraction < 0.01 {
            return Ok(());
        }
        let length = ((count + 1) as u32).next_power_of_two() << 2;
        table.enable_skew(length, length as usize * 8 + count * 4);
        for index in 0..count {
            let value = distribution.mcv_value(index, ctype)?;
            let Some(hash) = datum_hash(&value) else {
                return Err(crate::sql_err!(
                    crate::sql::eval::sqlstate::FEATURE_NOT_SUPPORTED,
                    "a hash join over keys of this type is not supported"
                ));
            };
            // Open addressing in the skew array; a repeated hash is skipped.
            let mut bucket = hash & (length - 1);
            loop {
                match table.skew_slot(bucket) {
                    Some(held) if held == hash => break,
                    Some(_) => bucket = (bucket + 1) & (length - 1),
                    None => {
                        table.take_skew_slot(SkewSlot {
                            position: bucket,
                            hash,
                            removed: false,
                        })?;
                        break;
                    }
                }
            }
        }
        Ok(())
    }

    /// The `t_len` of the minimal tuple of path `id`'s target list for
    /// the bound row.
    fn tuple_len(&self, bound: &[Option<BaseRow<'a>>], id: u32) -> Result<usize, SqlError> {
        self.tuple_sizes(bound, id, &[]).map(|(length, _)| length)
    }

    /// The columns of path `id`'s target list, then each of `extra` it
    /// lacks: a sort's key expressions, which `make_sort_from_pathkeys`
    /// adds to its input's target list as resjunk columns.
    fn sort_columns(
        &self,
        id: u32,
        extra: &[(usize, usize)],
        each: &mut dyn FnMut((usize, usize)),
    ) {
        let mut listed = [(usize::MAX, 0usize); super::index_paths::joins::MAX_KEYS];
        self.model
            .plan
            .target_list(self.model.plan.path(id).relids, &mut |column| {
                let member = (usize::from(column.rel), usize::from(column.column));
                for (slot, &key) in listed.iter_mut().zip(extra) {
                    if key == member {
                        *slot = key;
                    }
                }
                each(member);
            });
        for (at, &key) in extra.iter().enumerate() {
            if listed.get(at) != Some(&key) {
                each(key);
            }
        }
    }

    /// The `t_len` of a row's tuple under the `id` path, with the sort key
    /// columns `extra` it lacks, and what a sort writes of it to a tape
    /// (`writetup_heap`, or `writetup_datum` for a sort of one column).
    fn tuple_sizes(
        &self,
        bound: &[Option<BaseRow<'a>>],
        id: u32,
        extra: &[(usize, usize)],
    ) -> Result<(usize, usize), SqlError> {
        let mut values = [Datum::Null; 128];
        let mut width = 0;
        let mut fits = true;
        self.sort_columns(id, extra, &mut |member| {
            if width < values.len() {
                values[width] = self.value(bound, member);
                width += 1;
            } else {
                fits = false;
            }
        });
        let datum = !extra.is_empty() && width == 1;
        let sizes = fits
            .then(|| {
                super::postgres_memory::SizeSource {
                    storage: self.storage,
                    txid: self.txid,
                }
                .with(|sizing| {
                    let length = super::postgres_memory::minimal_tuple_len_with_tids(
                        Some(sizing),
                        values[..width].iter().copied(),
                        0,
                    )?;
                    let written = if datum {
                        4 + super::postgres_memory::value_written_size(
                            Some(sizing),
                            &values[0],
                            false,
                        )?
                    } else {
                        length - 10 + 4
                    };
                    Some((length, written))
                })
            })
            .flatten();
        sizes.ok_or_else(|| {
            crate::sql_err!(
                crate::sql::eval::sqlstate::FEATURE_NOT_SUPPORTED,
                "a join whose rows have no PostgreSQL size here is not supported"
            )
        })
    }
}

/// Scans `statement`'s FROM clause: by the plan PostgreSQL's join search
/// chose when it plans the query (`planned`, the statement's plan), otherwise
/// as `scan_source` does.
#[allow(clippy::too_many_arguments)]
pub(super) fn scan_statement_source<'a>(
    statement: &'a crate::sql::ast::Select<'a>,
    recycle_rows: bool,
    storage: &Storage,
    scope: &QueryScope<'a>,
    from: &'a crate::sql::ast::FromClause<'a>,
    txid: u32,
    where_clause: Option<&'a crate::sql::ast::Expr<'a>>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    hooks: &EvalHooks<'_, 'a>,
    outer: Option<&dyn ColumnLookup<'a>>,
    pax_demand: PaxReadDemand,
    planned: &PlannedJoin<'a>,
    visit: &mut dyn FnMut(&JoinRow<'_, 'a, '_>) -> Result<bool, SqlError>,
) -> Result<(), SqlError> {
    if outer.is_none()
        && let Some(model) = planned.get(storage, txid, statement, scope, params, arena)?
    {
        // A `GroupAggregate` over the join's rows sorted by its keys reads
        // them in its sort's order.
        if let (Some(group), Some(chosen)) = (model.group, model.plan.upper.group)
            && !group.distinct
            && let Some((_, _, presorted)) = chosen.input_sort
        {
            return run_modeled_sorted(
                storage, scope, txid, model, statement, presorted, arena, params, hooks, visit,
            );
        }
        return run_modeled(storage, scope, txid, model, arena, params, hooks, visit);
    }
    if recycle_rows {
        super::scan::scan_source_recycling_with_pax_columns(
            storage,
            scope,
            from,
            txid,
            where_clause,
            arena,
            params,
            hooks,
            outer,
            pax_demand,
            visit,
        )
    } else {
        super::scan::scan_source_with_pax_columns(
            storage,
            scope,
            from,
            txid,
            where_clause,
            arena,
            params,
            hooks,
            outer,
            pax_demand,
            visit,
        )
    }
}

/// Runs `model`'s plan and passes its rows to `visit` in the order the
/// `Sort` below its `GroupAggregate` returns them: PostgreSQL's tuplesort
/// by the grouping keys, or its incremental sort after the first
/// `presorted` of them, so each group's aggregates read their rows in
/// PostgreSQL's order.
#[allow(clippy::too_many_arguments)]
fn run_modeled_sorted<'a>(
    storage: &Storage,
    scope: &QueryScope<'a>,
    txid: u32,
    model: &ModeledJoin<'a>,
    statement: &'a crate::sql::ast::Select<'a>,
    presorted: u8,
    arena: &'a Arena,
    params: &[Datum<'a>],
    hooks: &EvalHooks<'_, 'a>,
    visit: &mut dyn FnMut(&JoinRow<'_, 'a, '_>) -> Result<bool, SqlError>,
) -> Result<(), SqlError> {
    let group = model.group.expect("a sorted grouping");
    // Each grouping key's column (`None` for an expression, which each row
    // evaluates), collation and direction.
    let mut keys =
        [(None::<(usize, usize)>, Collation::None, 0u8); super::index_paths::joins::MAX_KEYS];
    for (slot, &(position, flags)) in keys.iter_mut().zip(group.keys) {
        let expression = statement.group_by[position];
        let column = super::index_paths::join_query::column_of(scope, expression);
        let collation = scope.expression_collation(storage, arena, expression)?;
        *slot = (column, collation, flags);
    }
    let keys = &keys[..group.keys.len()];
    let mut held = crate::mem::arena::ArenaList::new(arena);
    let mut offsets: &[usize] = &[];
    run_modeled(
        storage,
        scope,
        txid,
        model,
        arena,
        params,
        hooks,
        &mut |row| {
            let JoinRowValues::Flat {
                values,
                offsets: row_offsets,
                forms: Some(row_forms),
            } = row.values
            else {
                unreachable!("a modeled join's rows are flat, with their forms")
            };
            offsets = arena
                .alloc_slice_copy(row_offsets)
                .map_err(|_| arena_full())?;
            let key_values = arena
                .alloc_slice_with(keys.len(), |_| Datum::Null)
                .map_err(|_| arena_full())?;
            for ((value, &(column, ..)), &(position, _)) in
                key_values.iter_mut().zip(keys).zip(group.keys)
            {
                *value = match column {
                    Some((table, column)) => values[row_offsets[table] + column],
                    None => crate::sql::eval::eval_full(
                        statement.group_by[position],
                        arena,
                        params,
                        row,
                        hooks,
                    )?,
                };
            }
            let values = arena.alloc_slice_copy(values).map_err(|_| arena_full())?;
            let forms = arena
                .alloc_slice_copy(row_forms)
                .map_err(|_| arena_full())?;
            let rowids = arena
                .alloc_slice_copy(row.rowids)
                .map_err(|_| arena_full())?;
            held.push((&*values, &*forms, &*rowids, &*key_values))
                .map_err(|_| arena_full())?;
            Ok(true)
        },
    )?;
    let rows = held.as_slice();
    let compare_key = |at: usize, left: u32, right: u32| {
        let (_, collation, flags) = keys[at];
        let (left, right) = (&rows[left as usize].3[at], &rows[right as usize].3[at]);
        let nulls_first = flags & super::index_paths::joins::KEY_NULLS_FIRST != 0;
        Ok::<_, SqlError>(match (left.is_null(), right.is_null()) {
            (true, true) => Ordering::Equal,
            (true, false) if nulls_first => Ordering::Less,
            (true, false) => Ordering::Greater,
            (false, true) if nulls_first => Ordering::Greater,
            (false, true) => Ordering::Less,
            (false, false) => {
                let compared = crate::sql::eval::compare_datums_collated(
                    storage, txid, collation, left, right,
                )?;
                if flags & super::index_paths::joins::KEY_DESCENDING != 0 {
                    compared.reverse()
                } else {
                    compared
                }
            }
        })
    };
    let compare = &mut |left: u32, right: u32| -> Result<Ordering, SqlError> {
        for key in 0..keys.len() {
            let ordering = compare_key(key, left, right)?;
            if !ordering.is_eq() {
                return Ok(ordering);
            }
        }
        Ok(Ordering::Equal)
    };
    let input = arena
        .alloc_slice_with(rows.len(), |at| at as u32)
        .map_err(|_| arena_full())?;
    let order: &[u32] = if presorted == 0 {
        crate::sql::pg_qsort::pg_tuplesort(input, None, compare)?;
        input
    } else {
        let prefix = usize::from(presorted);
        let scratch = arena
            .alloc_slice_with(rows.len() * 2, |_| 0u32)
            .map_err(|_| arena_full())?;
        let out = arena
            .alloc_slice_with(rows.len(), |_| 0u32)
            .map_err(|_| arena_full())?;
        crate::sql::pg_qsort::pg_incremental_sort(
            input,
            scratch,
            out,
            None,
            &mut |left, right| {
                for key in 0..prefix {
                    if !compare_key(key, left, right)?.is_eq() {
                        return Ok(false);
                    }
                }
                Ok(true)
            },
            compare,
            &mut |_, _, _| Ok(()),
        )?;
        out
    };
    for &at in order {
        let (values, forms, rowids, _) = rows[at as usize];
        if !visit(&JoinRow {
            scope,
            values: JoinRowValues::Flat {
                values,
                offsets,
                forms: Some(forms),
            },
            rowids,
            visible: scope.n,
        })? {
            break;
        }
    }
    Ok(())
}

/// Runs `model`'s plan, passing each joined row to `visit` until it
/// returns false.
#[allow(clippy::too_many_arguments)]
pub(super) fn run_modeled<'a>(
    storage: &Storage,
    scope: &QueryScope<'a>,
    txid: u32,
    model: &ModeledJoin<'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    hooks: &EvalHooks<'_, 'a>,
    visit: &mut dyn FnMut(&JoinRow<'_, 'a, '_>) -> Result<bool, SqlError>,
) -> Result<(), SqlError> {
    let written = scope;
    let block = super::instrument::current_block();
    let mut pull = ModeledPull::general(storage, txid, model, written, params, arena, hooks)?;
    while pull.next_row(storage, written, arena, hooks)? {
        let keep = {
            let _block = super::instrument::enter_block(block);
            visit(&pull.row(written))?
        };
        pull.finish_row();
        if !keep {
            break;
        }
    }
    pull.finish()
}

/// The joined row a plan returns, as the query reads it: each relation's
/// bound row, a removed self-join's relation reading the row of the scan
/// kept in its place and a removed join's relation reading NULLs; with
/// parenthesized joins, each written entry's row built from its members'.
/// Its buffers are allocated once and refilled for each row.
pub(super) struct OutputRow<'a> {
    offsets: &'a [usize],
    widths: &'a [usize],
    values: &'a mut [Datum<'a>],
    forms: &'a mut [StoredForm],
    rowids: &'a mut [Option<u64>],
    written_offsets: &'a [usize],
    written_values: &'a mut [Datum<'a>],
    written_forms: &'a mut [StoredForm],
    written_rowids: &'a mut [Option<u64>],
}

impl<'a> OutputRow<'a> {
    /// The buffers of `model`'s rows over the query's `written` scope.
    pub(super) fn new(
        model: &ModeledJoin<'a>,
        written: &QueryScope<'a>,
        arena: &'a Arena,
    ) -> Result<Self, SqlError> {
        // The plan's relations; a parenthesized join's members fill its
        // written entry's columns in order, so one flat row serves both.
        let scope = model.scope(written);
        let n = scope.n;
        let written_offsets = arena
            .alloc_slice_with(written.n, |_| 0usize)
            .map_err(|_| arena_full())?;
        let mut written_width = 0;
        for (table, offset) in written_offsets.iter_mut().enumerate() {
            *offset = written_width;
            written_width += written.stored_width(table);
        }
        let offsets = arena
            .alloc_slice_with(n, |_| 0usize)
            .map_err(|_| arena_full())?;
        let mut width = 0;
        for (table, offset) in offsets.iter_mut().enumerate() {
            *offset = width;
            width += scope.stored_width(table);
        }
        Ok(Self {
            offsets,
            widths: arena
                .alloc_slice_with(n, |table| scope.stored_width(table))
                .map_err(|_| arena_full())?,
            values: arena
                .alloc_slice_with(width, |_| Datum::Null)
                .map_err(|_| arena_full())?,
            forms: arena
                .alloc_slice_with(width, |_| StoredForm::Inline)
                .map_err(|_| arena_full())?,
            rowids: arena
                .alloc_slice_with(n, |_| None::<u64>)
                .map_err(|_| arena_full())?,
            written_offsets,
            written_values: arena
                .alloc_slice_with(written_width, |_| Datum::Null)
                .map_err(|_| arena_full())?,
            written_forms: arena
                .alloc_slice_with(written_width, |_| StoredForm::Inline)
                .map_err(|_| arena_full())?,
            written_rowids: arena
                .alloc_slice_with(written.n, |_| None::<u64>)
                .map_err(|_| arena_full())?,
        })
    }

    /// Copies the top join's bound rows in.
    fn fill(
        &mut self,
        model: &ModeledJoin<'a>,
        written: &QueryScope<'a>,
        nulls: &[&'a [Datum<'a>]],
        bound: &[Option<BaseRow<'a>>],
    ) {
        let Self {
            offsets,
            widths,
            values,
            forms,
            rowids,
            written_offsets,
            written_values,
            written_forms,
            written_rowids,
        } = self;
        let n = offsets.len();
        forms.fill(StoredForm::Inline);
        for table in 0..n {
            let start = offsets[table];
            let columns = widths[table];
            // A removed self-join's relation reads the row of the scan kept
            // in its place.
            if let Some(kept) = model.kept_for[table] {
                let row = bound[usize::from(kept)].expect("the top join binds every relation");
                values[start..start + columns].copy_from_slice(&row.values[..columns]);
                forms[start..start + row.forms.len()].copy_from_slice(row.forms);
                rowids[table] = row.rowid;
                continue;
            }
            // A removed join's relation reads as NULLs, which nothing reads.
            if model.plan.removed & Relids::of(table) != Relids::EMPTY {
                values[start..start + columns].copy_from_slice(&nulls[table][..columns]);
                rowids[table] = None;
                continue;
            }
            let row = bound[table].expect("the top join binds every relation");
            values[start..start + columns].copy_from_slice(&row.values[..columns]);
            forms[start..start + row.forms.len()].copy_from_slice(row.forms);
            rowids[table] = row.rowid;
        }
        let Some(nested) = model.nested.as_ref() else {
            return;
        };
        // Each written entry's row from its members' rows; a parenthesized
        // join's is a joined row of no one row.
        let mut leaf = 0;
        for table in 0..written.n {
            let count = (leaf..n)
                .take_while(|&member| usize::from(nested.entries[member]) == table)
                .count();
            let mut at = written_offsets[table];
            let members = leaf..leaf + count;
            leaf += count;
            if !nested.joined.contains(table as u32) {
                let width = widths[members.start];
                written_values[at..at + width]
                    .copy_from_slice(&values[offsets[members.start]..][..width]);
                written_forms[at..at + width]
                    .copy_from_slice(&forms[offsets[members.start]..][..width]);
                written_rowids[table] = rowids[members.start];
                continue;
            }
            written_rowids[table] = None;
            for _ in 0..2 {
                for member in members.clone() {
                    let width = widths[member];
                    written_values[at..at + width]
                        .copy_from_slice(&values[offsets[member]..][..width]);
                    written_forms[at..at + width]
                        .copy_from_slice(&forms[offsets[member]..][..width]);
                    at += width;
                }
            }
            for member in members {
                let nulled =
                    bound[member].is_none_or(|row| core::ptr::eq(row.values, nulls[member]));
                written_values[at] = Datum::Int4(i32::from(nulled));
                written_forms[at] = StoredForm::Inline;
                at += 1;
            }
        }
    }

    /// The row last filled, over the plan's relations or, with
    /// parenthesized joins, over `written`.
    pub(super) fn row<'s>(
        &'s self,
        model: &'s ModeledJoin<'a>,
        written: &'s QueryScope<'a>,
    ) -> JoinRow<'s, 'a, 'a> {
        match model.nested.as_ref() {
            None => JoinRow {
                scope: written,
                values: JoinRowValues::Flat {
                    values: self.values,
                    offsets: self.offsets,
                    forms: Some(self.forms),
                },
                rowids: self.rowids,
                visible: written.n,
            },
            Some(_) => JoinRow {
                scope: written,
                values: JoinRowValues::Flat {
                    values: self.written_values,
                    offsets: self.written_offsets,
                    forms: Some(self.written_forms),
                },
                rowids: self.written_rowids,
                visible: written.n,
            },
        }
    }
}

/// A base relation's scan node and the outer row its condition reads.
struct ScanRecord<'a> {
    node: super::scan::PathScan<'a>,
    /// The relations whose values the scan's condition reads.
    param: Relids,
    /// Their bound values, flat by the query's FROM positions, and those
    /// values' stored forms; the other relations' values are NULL.
    outer: &'a mut [Datum<'a>],
    outer_forms: &'a mut [StoredForm],
}

fn node_pool_full(error: crate::mem::lease_pool::LeaseError) -> SqlError {
    crate::sql_err!(
        crate::sql::eval::sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "a join's plan nodes exceed plan_build_bytes: {}",
        error
    )
    .plan_build_exhausted()
}

/// The orders the nodes read stored rows in, each over the rows decoded
/// into the arena.
impl<'r, 'a> JoinContext<'r, 'a> {
    /// The bound rows of `relids`, flat by the query's FROM positions: the
    /// outer row a parameterized scan or a `LATERAL` subquery reads.
    fn outer_values(
        &self,
        bound: &[Option<BaseRow<'a>>],
        relids: Relids,
    ) -> Result<OuterValues<'a>, SqlError> {
        let n = self.scope.n;
        let offsets = self
            .arena
            .alloc_slice_with(n, |_| 0usize)
            .map_err(|_| arena_full())?;
        let mut width = 0;
        for (table, offset) in offsets.iter_mut().enumerate() {
            *offset = width;
            width += self.scope.stored_width(table);
        }
        let values = self
            .arena
            .alloc_slice_with(width, |_| Datum::Null)
            .map_err(|_| arena_full())?;
        for table in 0..n {
            if let Some(row) = bound[table]
                && relids.contains(table as u32)
            {
                let columns = self.columns_of(row.values);
                values[offsets[table]..offsets[table] + columns.len()].copy_from_slice(columns);
            }
        }
        let forms = self
            .arena
            .alloc_slice_with(width, |_| StoredForm::Inline)
            .map_err(|_| arena_full())?;
        self.flat_forms(bound, Some(relids), forms);
        Ok((values, offsets, forms))
    }

    /// How the index scan of `rel` by its path `path`, read in part by a
    /// merge join, counts its pages.
    fn lazy_pages(&self, rel: usize, path: u16, buffers: bool) -> Result<LazyPages<'a>, SqlError> {
        let modeled = self.model.rels[rel];
        let scan = modeled.scan(self.storage, self.txid, path, self.params, self.arena)?;
        let slot = self.scope.slots[rel];
        let index_only = scan.path.kind == super::index_paths::ScanKind::IndexOnly;
        if !buffers && !index_only {
            return Ok(LazyPages {
                model: super::instrument::PageModel {
                    local: false,
                    relation_pages: 0,
                    rows: 0,
                    index: None,
                    sequential: false,
                    visibility_map: false,
                },
                heap: &[],
                visible: None,
                buffers,
            });
        }
        let (heap, relation_pages) =
            super::instrument::heap_pages(self.storage, slot, self.txid, self.arena)?;
        let visible =
            index_only.then(|| super::instrument::all_visible_pages(self.storage, slot, self.txid));
        Ok(LazyPages {
            model: super::instrument::PageModel {
                local: self.storage.table_def(slot, self.txid).persistence
                    == crate::storage::RelationPersistence::Temporary,
                relation_pages,
                rows: heap.len() as u64,
                index: Some(scan.path.index_pages),
                sequential: false,
                visibility_map: visible.is_some_and(|visible| visible > 0),
            },
            heap,
            visible,
            buffers,
        })
    }

    /// Whether the join can count an index scan of `rel` read in part: an
    /// index or index-only scan without a filter, whose rows are each one
    /// fetch.
    fn counts_lazily(&self, rel: usize, path: u16) -> Result<bool, SqlError> {
        let modeled = self.model.rels[rel];
        let scan = modeled.scan(self.storage, self.txid, path, self.params, self.arena)?;
        let conjuncts = modeled.restriction.map_or(0, |restriction| {
            let mut parts = [&crate::sql::ast::Expr::Null; super::plan::MAX_CONJUNCTS];
            let mut count = 0;
            super::plan::flatten_and(restriction, &mut parts, &mut count);
            count
        });
        Ok(matches!(
            scan.path.kind,
            super::index_paths::ScanKind::Index | super::index_paths::ScanKind::IndexOnly
        ) && scan.path.absorbed.len() == conjuncts)
    }

    /// Whether two Memoize keys are equal: NULLs equal each other.
    fn same_key(
        &self,
        held: &[Datum<'a>],
        key: &[Datum<'a>],
        members: &[super::index_paths::joins::Member],
        binary: bool,
    ) -> Result<bool, SqlError> {
        for ((left, right), member) in held.iter().zip(key).zip(members) {
            match (left.is_null(), right.is_null()) {
                (true, true) => continue,
                (true, false) | (false, true) => return Ok(false),
                (false, false) => {}
            }
            let collation = self.collation((usize::from(member.rel), usize::from(member.column)));
            if crate::sql::eval::compare_datums_collated(
                self.storage,
                self.txid,
                collation,
                left,
                right,
            )? != Ordering::Equal
            {
                return Ok(false);
            }
            // Binary mode (`datum_image_eq`) tells apart equal values whose
            // stored images differ: numerics of different scales, zeros of
            // either sign, intervals of different units and times in
            // different zones.
            if binary && !same_image(left, right) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// `ExecInitSort`: a sort by `keys` of path `id`'s rows whose tuple is
    /// a single column sorts datums.
    fn sorts_datums(&self, id: u32, keys: &[(usize, usize)]) -> bool {
        let mut columns = 0;
        self.sort_columns(id, keys, &mut |_| columns += 1);
        columns == 1
    }

    /// The sizes a tuplesort by `keys` of path `id`'s rows charges for the
    /// bound row: its tuple's `t_len` and what a run writes of it.
    fn sort_sizes(
        &self,
        row: &[Option<BaseRow<'a>>],
        id: u32,
        keys: &[(usize, usize)],
    ) -> Result<(usize, usize), SqlError> {
        self.tuple_sizes(row, id, keys)
    }

    /// The members a sort by `keys` of a path of `relids` compares, each
    /// with its key's flags.
    fn sort_members(&self, keys: &Keys, relids: Relids) -> ([(usize, usize); 8], [u8; 8], usize) {
        let mut members = [(0usize, 0usize); 8];
        let mut flags = [0u8; 8];
        for (at, &key) in keys.as_slice().iter().enumerate() {
            let class = key & KEY_CLASS;
            let member = self.model.plan.classes[usize::from(class)]
                .sort_member(relids)
                .expect("a sort key reads its input");
            members[at] = self.model.member(class, member as u8);
            flags[at] = key;
        }
        (members, flags, keys.as_slice().len())
    }

    /// Each compared member's column, for its collation.
    fn unique_members(
        keys: &[super::index_paths::joins::Member],
    ) -> ([(usize, usize); super::index_paths::joins::MAX_KEYS], usize) {
        let mut members = [(0usize, 0usize); super::index_paths::joins::MAX_KEYS];
        for (slot, member) in members.iter_mut().zip(keys) {
            *slot = (usize::from(member.rel), usize::from(member.column));
        }
        (members, keys.len())
    }

    /// A `Unique` over a `Sort` of a semi join's compared columns, whose
    /// values each row of `keys` holds: the rows' positions sorted by them
    /// (`pg_qsort`, NULLs last), the first of each run of equal values,
    /// NULLs equal (`execTuplesUnique`).
    fn unique_order(
        &self,
        input: u32,
        keys: &[&[Datum<'a>]],
        sizes: &[(usize, usize)],
        members: &[(usize, usize)],
    ) -> Result<&'a [u32], SqlError> {
        let compare = |a: u32, b: u32| -> Result<Ordering, SqlError> {
            for (at, &member) in members.iter().enumerate() {
                let (one, two) = (&keys[a as usize][at], &keys[b as usize][at]);
                let order = match (one.is_null(), two.is_null()) {
                    (true, true) => Ordering::Equal,
                    (true, false) => Ordering::Greater,
                    (false, true) => Ordering::Less,
                    (false, false) => crate::sql::eval::compare_datums_collated(
                        self.storage,
                        self.txid,
                        self.collation(member),
                        one,
                        two,
                    )?,
                };
                if order != Ordering::Equal {
                    return Ok(order);
                }
            }
            Ok(Ordering::Equal)
        };
        let (order, _) = self.tuplesort((input, members), sizes, &mut |a, b| compare(a, b))?;
        let kept = self
            .arena
            .alloc_slice_with(order.len(), |_| 0u32)
            .map_err(|_| arena_full())?;
        let mut count = 0;
        for &at in order {
            if count > 0 && compare(kept[count - 1], at)? == Ordering::Equal {
                continue;
            }
            kept[count] = at;
            count += 1;
        }
        Ok(&kept[..count])
    }

    /// A `HashAggregate` making a semi join's inner relation unique: the
    /// positions of the first row of each group of equal compared columns,
    /// whose values each row of `keys` holds (NULLs equal), in the order
    /// `ExecAgg` walks its `simplehash` table (`TupleHashTableHash`'s
    /// `murmurhash32` of the combined hashes, robin hood insertion, growth
    /// by doubling, `SH_START_ITERATE` from the first empty bucket
    /// backwards).
    fn unique_hashed_order(
        &self,
        input: u32,
        keys: &[&[Datum<'a>]],
        members: &[(usize, usize)],
        groups: f64,
    ) -> Result<&'a [u32], SqlError> {
        let width = self.model.plan.path(input).width;
        let row_hash = |row: usize| -> Result<u32, SqlError> {
            let mut hash = 0u32;
            for (at, value) in keys[row].iter().enumerate() {
                if at != 0 {
                    hash = hash.rotate_left(1);
                }
                if value.is_null() {
                    continue;
                }
                hash ^= datum_hash(value).ok_or_else(|| {
                    crate::sql_err!(
                        crate::sql::eval::sqlstate::FEATURE_NOT_SUPPORTED,
                        "a hashed unique-ified relation over keys of this type is not supported"
                    )
                })?;
            }
            Ok(super::simplehash::murmur32(hash))
        };
        let same = |a: usize, b: usize| -> Result<bool, SqlError> {
            for (at, &member) in members.iter().enumerate() {
                let (one, two) = (&keys[a][at], &keys[b][at]);
                let equal = match (one.is_null(), two.is_null()) {
                    (true, true) => true,
                    (false, false) => {
                        crate::sql::eval::compare_datums_collated(
                            self.storage,
                            self.txid,
                            self.collation(member),
                            one,
                            two,
                        )? == Ordering::Equal
                    }
                    _ => false,
                };
                if !equal {
                    return Ok(false);
                }
            }
            Ok(true)
        };
        let sizing = super::index_paths::plain_hash_sizing(width, groups, members.len())
            .ok_or_else(|| {
                crate::sql_err!(
                    crate::sql::eval::sqlstate::FEATURE_NOT_SUPPORTED,
                    "a hashed unique-ified relation over {} columns is not supported",
                    members.len()
                )
            })?;
        let hashes = self
            .arena
            .alloc_slice_with(keys.len(), |_| 0u32)
            .map_err(|_| arena_full())?;
        let lens = self
            .arena
            .alloc_slice_with(keys.len(), |_| 0usize)
            .map_err(|_| arena_full())?;
        for row in 0..keys.len() {
            hashes[row] = row_hash(row)?;
            lens[row] = super::postgres_memory::minimal_tuple_len_with_tids(
                None,
                keys[row].iter().copied(),
                0,
            )
            .ok_or_else(super::instrument::unsized_value)?;
        }
        let order = self
            .arena
            .alloc_slice_with(keys.len(), |_| 0u32)
            .map_err(|_| arena_full())?;
        let (count, _) = super::hashagg::hashed_row_order(
            self.storage.plan_build(),
            sizing,
            hashes,
            lens,
            same,
            order,
        )?;
        Ok(&order[..count])
    }

    /// The order of rows whose sort key values each row of `keys` holds
    /// after a sort by `members` with `flags` (PostgreSQL's tuplesort,
    /// NULLs last unless a key puts them first), and the space the sort
    /// reports.
    fn sort_order(
        &self,
        id: u32,
        keys: &[&[Datum<'a>]],
        sizes: &[(usize, usize)],
        (members, flags): (&[(usize, usize)], &[u8]),
    ) -> Result<(&'a [u32], super::instrument::SortSpace), SqlError> {
        self.tuplesort((id, members), sizes, &mut |a, b| {
            for (at, (&member, &key)) in members.iter().zip(flags).enumerate() {
                let (one, two) = (&keys[a as usize][at], &keys[b as usize][at]);
                let first = key & KEY_NULLS_FIRST != 0;
                let order = match (one.is_null(), two.is_null()) {
                    (true, true) => Ordering::Equal,
                    (true, false) if first => Ordering::Less,
                    (true, false) => Ordering::Greater,
                    (false, true) if first => Ordering::Greater,
                    (false, true) => Ordering::Less,
                    (false, false) => {
                        let order = crate::sql::eval::compare_datums_collated(
                            self.storage,
                            self.txid,
                            self.collation(member),
                            one,
                            two,
                        )?;
                        if key & KEY_DESCENDING != 0 {
                            order.reverse()
                        } else {
                            order
                        }
                    }
                };
                if order != Ordering::Equal {
                    return Ok(order);
                }
            }
            Ok(Ordering::Equal)
        })
    }

    /// PostgreSQL's tuplesort of the `id` path's rows, of `sizes`, in arrival order, as
    /// `tuplesort.c` runs it: each tuple (the path's target list) is charged
    /// as it arrives, a sort that outgrows `work_mem` forms its runs where
    /// `dumptuples` does and merges them as `mergeruns` does, any other is
    /// one `pg_qsort`. Rows `compare` ranks equal come out in that order.
    fn tuplesort(
        &self,
        (id, keys): (u32, &[(usize, usize)]),
        sizes: &[(usize, usize)],
        compare: &mut dyn FnMut(u32, u32) -> Result<Ordering, SqlError>,
    ) -> Result<(&'a [u32], super::instrument::SortSpace), SqlError> {
        use super::postgres_memory::{SortTuples, TuplesortModel};
        let mut model = TuplesortModel::new(
            if self.sorts_datums(id, keys) {
                SortTuples::Datum
            } else {
                SortTuples::Bump
            },
            None,
        );
        // The row counts of the runs `dumptuples` wrote.
        let runs = self
            .arena
            .alloc_slice_with(sizes.len() + 1, |_| 0u32)
            .map_err(|_| arena_full())?;
        let mut run_count = 0usize;
        for &(length, written) in sizes {
            if let Some(run) = model.push(length, written) {
                runs[run_count] = run as u32;
                run_count += 1;
            }
        }
        if let Some(run) = model.finish_run() {
            runs[run_count] = run as u32;
            run_count += 1;
        }
        let order = self
            .arena
            .alloc_slice_with(sizes.len(), |at| at as u32)
            .map_err(|_| arena_full())?;
        if let Some(merge_order) = model.merge_order() {
            let scratch = crate::sql::pg_qsort::RunScratch {
                moved: self
                    .arena
                    .alloc_slice_with(sizes.len(), |_| 0u32)
                    .map_err(|_| arena_full())?,
                bounds: self
                    .arena
                    .alloc_slice_with(2 * (run_count + 1), |_| 0u32)
                    .map_err(|_| arena_full())?,
                heap: self
                    .arena
                    .alloc_slice_with(merge_order, |_| 0u32)
                    .map_err(|_| arena_full())?,
                cursor: self
                    .arena
                    .alloc_slice_with(merge_order, |_| 0u32)
                    .map_err(|_| arena_full())?,
            };
            crate::sql::pg_qsort::pg_tuplesort_runs(
                order,
                &runs[..run_count],
                merge_order,
                scratch,
                compare,
            )?;
        } else {
            crate::sql::pg_qsort::pg_tuplesort(order, None, compare)?;
        }
        Ok((
            order,
            super::instrument::SortSpace::from_postgres(model.space(None), false),
        ))
    }
}
