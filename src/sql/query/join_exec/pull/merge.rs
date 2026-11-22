//! A merge join as `nodeMergejoin.c` runs it, one row per call: its
//! states are `ExecMergeJoin`'s, and a row returned in a state resumes in
//! it, as PostgreSQL's does.
//!
//! An outer input in merge order is read from its path a row at a time; a
//! sorted one is read whole into the node's store at its first fetch and
//! returned in its sort's order. The inner input is always kept in a store,
//! sorted or in its own order, so the join marks and restores a position
//! in it (`ExecMarkPos`, `ExecRestrPos`): its rows are decoded into the
//! arena above the current outer row, whose next read releases them.

use super::*;

/// `ExecMergeJoin`'s states, and the end PostgreSQL returns NULL at.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    InitializeOuter,
    InitializeInner,
    JoinTuples,
    NextInner,
    NextOuter,
    TestOuter,
    SkipTest,
    SkipOuterAdvance,
    SkipInnerAdvance,
    EndOuter,
    EndInner,
    Done,
}

/// A merge join's state between rows.
pub(super) struct MergeRecord<'a> {
    phase: Phase,
    /// The outer input's rows when a sort orders them; an input in merge
    /// order is read from its path.
    outer_sorted: Option<OrderedRows>,
    inner: OrderedRows,
    /// Whether an outer row is current (`mj_OuterTupleSlot`), and a sorted
    /// outer input's next row.
    outer_current: bool,
    outer_next: usize,
    /// The current inner row (`mj_InnerTupleSlot`), the next one a fetch
    /// returns, and the marked one (`mj_MarkedTupleSlot`).
    inner_at: Option<usize>,
    inner_next: usize,
    marked: Option<usize>,
    matched_outer: bool,
    matched_inner: bool,
    /// The arena's frontier before a sorted outer row, and before the
    /// inner row bound in `inner_row`.
    outer_mark: Option<ArenaMark>,
    inner_mark: Option<ArenaMark>,
    inner_loaded: Option<usize>,
    /// The current outer and inner rows' relations, bound again after a
    /// row with NULLs for one side returns.
    outer_row: &'a mut [Option<BaseRow<'a>>],
    inner_row: &'a mut [Option<BaseRow<'a>>],
}

impl<'a> MergeRecord<'a> {
    pub(super) fn new(
        pool: &Arc<LeasePool>,
        (outer_width, outer_sorted): (usize, bool),
        inner_width: usize,
        relations: usize,
        arena: &'a Arena,
    ) -> Result<Self, SqlError> {
        Ok(Self {
            phase: Phase::InitializeOuter,
            outer_sorted: outer_sorted
                .then(|| OrderedRows::new(pool, outer_width))
                .transpose()?,
            inner: OrderedRows::new(pool, inner_width)?,
            outer_current: false,
            outer_next: 0,
            inner_at: None,
            inner_next: 0,
            marked: None,
            matched_outer: false,
            matched_inner: false,
            outer_mark: None,
            inner_mark: None,
            inner_loaded: None,
            outer_row: arena
                .alloc_slice_with(relations, |_| None)
                .map_err(|_| arena_full())?,
            inner_row: arena
                .alloc_slice_with(relations, |_| None)
                .map_err(|_| arena_full())?,
        })
    }

    /// `ExecReScanMergeJoin`: the join starts again; a sorted input whose
    /// rows depend on an outer row above (`chgParam`) is read again, any
    /// other returns its kept rows from the first. Returns whether the
    /// outer and the inner store were emptied, whose paths are read again.
    pub(super) fn restart(&mut self, reread_outer: bool, reread_inner: bool) -> (bool, bool) {
        self.phase = Phase::InitializeOuter;
        self.outer_current = false;
        self.outer_next = 0;
        self.inner_at = None;
        self.inner_next = 0;
        self.marked = None;
        self.matched_outer = false;
        self.matched_inner = false;
        self.outer_mark = None;
        self.inner_mark = None;
        self.inner_loaded = None;
        // A store whose input was not read to its end is read again too.
        let outer_cleared = match &mut self.outer_sorted {
            Some(outer) if reread_outer || !outer.filled => {
                outer.clear();
                true
            }
            Some(_) => false,
            None => true,
        };
        let inner_cleared = reread_inner || !self.inner.filled;
        if inner_cleared {
            self.inner.clear();
        }
        (outer_cleared, inner_cleared)
    }
}

fn merge_record<'r, 'a>(records: &'r mut [Record<'a>], id: u32) -> &'r mut MergeRecord<'a> {
    let Record::MergeJoin(record) = &mut records[id as usize] else {
        unreachable!("a merge join's record");
    };
    record
}

/// What one merge join reads, from its path.
pub(super) struct MergeJoin<'k> {
    pub(super) jointype: JoinType,
    pub(super) outer: u32,
    pub(super) inner: u32,
    pub(super) outer_sort: Keys,
    pub(super) inner_sort: Keys,
    pub(super) keys: KeySides,
    pub(super) join_quals: &'k ClauseList,
    pub(super) other_quals: &'k ClauseList,
    pub(super) single_match: bool,
    pub(super) skip_mark_restore: bool,
    /// Bit `i` for merge clause `i`.
    pub(super) descending: u32,
    pub(super) nulls_first: u32,
}

impl<'a> Step<'_, '_, 'a> {
    /// Moves merge join `id` to its next row (`ExecMergeJoin`).
    pub(super) fn merge_join(&mut self, id: u32, join: &MergeJoin<'_>) -> Result<bool, SqlError> {
        let cx = self.cx;
        let fill_outer = join.jointype.fills_outer();
        let fill_inner = join.jointype.fills_inner();
        let out_of_order = || {
            crate::sql_err!(
                crate::sql::eval::sqlstate::INTERNAL_ERROR,
                "mergejoin input data is out of order"
            )
        };
        loop {
            super::super::super::check_timeout()?;
            let record = merge_record(self.records, id);
            match record.phase {
                Phase::Done => return Ok(false),
                Phase::InitializeOuter => match self.fetch_outer(id, join)? {
                    MergeEval::Matchable => self.set_phase(id, Phase::InitializeInner),
                    MergeEval::NonMatchable => {
                        if fill_outer && self.fill_outer(id, join)? {
                            return Ok(true);
                        }
                    }
                    MergeEval::EndOfJoin => {
                        if !fill_inner {
                            self.set_phase(id, Phase::Done);
                            return Ok(false);
                        }
                        let record = merge_record(self.records, id);
                        record.phase = Phase::EndOuter;
                        record.matched_inner = true;
                    }
                },
                Phase::InitializeInner => match self.fetch_inner(id, join)? {
                    MergeEval::Matchable => self.set_phase(id, Phase::SkipTest),
                    MergeEval::NonMatchable => {
                        if fill_inner && self.fill_inner(id, join)? {
                            return Ok(true);
                        }
                    }
                    MergeEval::EndOfJoin => {
                        if !fill_outer {
                            self.set_phase(id, Phase::Done);
                            return Ok(false);
                        }
                        let record = merge_record(self.records, id);
                        record.phase = Phase::EndInner;
                        record.matched_outer = false;
                    }
                },
                Phase::JoinTuples => {
                    record.phase = Phase::NextInner;
                    let inner_at = record.inner_at.expect("a current inner row");
                    self.bind_outer(id, join);
                    self.bind_inner(id, join, inner_at)?;
                    if !cx.equal(self.bound, join.join_quals)? {
                        continue;
                    }
                    let record = merge_record(self.records, id);
                    record.matched_outer = true;
                    record.matched_inner = true;
                    if join.jointype == JoinType::Anti {
                        record.phase = Phase::NextOuter;
                        continue;
                    }
                    if join.single_match {
                        record.phase = Phase::NextOuter;
                    }
                    if join.jointype == JoinType::RightAnti {
                        continue;
                    }
                    if cx.equal(self.bound, join.other_quals)? {
                        return Ok(true);
                    }
                }
                Phase::NextInner => {
                    if fill_inner && !record.matched_inner {
                        record.matched_inner = true;
                        if self.fill_inner(id, join)? {
                            return Ok(true);
                        }
                    }
                    let fetched = self.fetch_inner(id, join)?;
                    let record = merge_record(self.records, id);
                    record.matched_inner = false;
                    let phase = match fetched {
                        MergeEval::Matchable => match self.compare(id, join, None)? {
                            Ordering::Equal => Phase::JoinTuples,
                            Ordering::Less => Phase::NextOuter,
                            Ordering::Greater => return Err(out_of_order()),
                        },
                        MergeEval::NonMatchable => Phase::NextOuter,
                        MergeEval::EndOfJoin => {
                            record.inner_at = None;
                            Phase::NextOuter
                        }
                    };
                    self.set_phase(id, phase);
                }
                Phase::NextOuter | Phase::SkipOuterAdvance => {
                    let phase = record.phase;
                    if fill_outer && !record.matched_outer {
                        record.matched_outer = true;
                        if self.fill_outer(id, join)? {
                            return Ok(true);
                        }
                    }
                    let fetched = self.fetch_outer(id, join)?;
                    let record = merge_record(self.records, id);
                    record.matched_outer = false;
                    match fetched {
                        MergeEval::Matchable => {
                            record.phase = if phase == Phase::NextOuter {
                                Phase::TestOuter
                            } else {
                                Phase::SkipTest
                            };
                        }
                        MergeEval::NonMatchable => {}
                        MergeEval::EndOfJoin => {
                            if fill_inner && record.inner_at.is_some() {
                                record.phase = Phase::EndOuter;
                            } else {
                                record.phase = Phase::Done;
                                return Ok(false);
                            }
                        }
                    }
                }
                Phase::TestOuter => {
                    // The outer row against the marked inner row.
                    let marked = record.marked.expect("a marked inner row");
                    match self.compare(id, join, Some(marked))? {
                        Ordering::Equal => {
                            let record = merge_record(self.records, id);
                            if !join.skip_mark_restore {
                                // `ExecRestrPos`: the next fetch follows the
                                // mark.
                                record.inner_at = Some(marked);
                                record.inner_next = marked + 1;
                            }
                            record.phase = Phase::JoinTuples;
                        }
                        Ordering::Greater => {
                            let current = match merge_record(self.records, id).inner_at {
                                None => MergeEval::EndOfJoin,
                                Some(at) => {
                                    self.bind_inner(id, join, at)?;
                                    self.eval(join, false)
                                }
                            };
                            let phase = match current {
                                MergeEval::Matchable => Phase::SkipTest,
                                MergeEval::NonMatchable => Phase::SkipInnerAdvance,
                                MergeEval::EndOfJoin if fill_outer => Phase::EndInner,
                                MergeEval::EndOfJoin => {
                                    self.set_phase(id, Phase::Done);
                                    return Ok(false);
                                }
                            };
                            self.set_phase(id, phase);
                        }
                        Ordering::Less => return Err(out_of_order()),
                    }
                }
                Phase::SkipTest => {
                    let phase = match self.compare(id, join, None)? {
                        Ordering::Equal => {
                            let record = merge_record(self.records, id);
                            record.marked = record.inner_at;
                            Phase::JoinTuples
                        }
                        Ordering::Less => Phase::SkipOuterAdvance,
                        Ordering::Greater => Phase::SkipInnerAdvance,
                    };
                    self.set_phase(id, phase);
                }
                Phase::SkipInnerAdvance => {
                    if fill_inner && !record.matched_inner {
                        record.matched_inner = true;
                        if self.fill_inner(id, join)? {
                            return Ok(true);
                        }
                    }
                    let fetched = self.fetch_inner(id, join)?;
                    let record = merge_record(self.records, id);
                    record.matched_inner = false;
                    match fetched {
                        MergeEval::Matchable => record.phase = Phase::SkipTest,
                        MergeEval::NonMatchable => {}
                        MergeEval::EndOfJoin => {
                            if fill_outer && record.outer_current {
                                record.phase = Phase::EndInner;
                            } else {
                                record.phase = Phase::Done;
                                return Ok(false);
                            }
                        }
                    }
                }
                Phase::EndOuter => {
                    if !record.matched_inner {
                        record.matched_inner = true;
                        if self.fill_inner(id, join)? {
                            return Ok(true);
                        }
                    }
                    self.fetch_inner(id, join)?;
                    let record = merge_record(self.records, id);
                    record.matched_inner = false;
                    if record.inner_at.is_none() {
                        record.phase = Phase::Done;
                        return Ok(false);
                    }
                }
                Phase::EndInner => {
                    if !record.matched_outer {
                        record.matched_outer = true;
                        if self.fill_outer(id, join)? {
                            return Ok(true);
                        }
                    }
                    self.fetch_outer(id, join)?;
                    let record = merge_record(self.records, id);
                    record.matched_outer = false;
                    if !record.outer_current {
                        record.phase = Phase::Done;
                        return Ok(false);
                    }
                }
            }
        }
    }

    fn set_phase(&mut self, id: u32, phase: Phase) {
        merge_record(self.records, id).phase = phase;
    }

    /// Reads the next outer row (`ExecProcNode` of the outer plan) and
    /// evaluates its keys (`MJEvalOuterValues`). A wait resumes the read.
    fn fetch_outer(&mut self, id: u32, join: &MergeJoin<'_>) -> Result<MergeEval, SqlError> {
        let cx = self.cx;
        let outer_relids = cx.model.plan.path(join.outer).relids;
        // The outer read releases every inner row decoded above the
        // current outer row.
        let record = merge_record(self.records, id);
        record.inner_mark = None;
        record.inner_loaded = None;
        let present = if record.outer_sorted.is_some() {
            self.fill(id, join.outer, |record| merge_record_rows(record, true))?;
            let record = merge_record(self.records, id);
            let outer = record.outer_sorted.as_mut().expect("a sorted outer input");
            if !outer.arranged {
                let (members, flags, count) = cx.sort_members(&join.outer_sort, outer_relids);
                let keys = (&members[..count], &flags[..count]);
                outer.arrange(cx, keys.0, join.outer, |rows, sizes| {
                    cx.sort_order(join.outer, rows, sizes, keys)
                        .map(|(order, _)| order)
                })?;
            }
            if record.outer_next < outer.len() {
                Step::release(cx.arena, &mut record.outer_mark);
                outer.load(record.outer_next, cx, self.bound)?;
                record.outer_next += 1;
                true
            } else {
                false
            }
        } else {
            self.next(join.outer)?
        };
        let record = merge_record(self.records, id);
        record.outer_current = present;
        if !present {
            return Ok(MergeEval::EndOfJoin);
        }
        cx.bind(record.outer_row, self.bound, outer_relids);
        Ok(self.eval(join, true))
    }

    /// Moves to the next inner row (`ExecProcNode` of the inner plan) and
    /// evaluates its keys (`MJEvalInnerValues`). The inner input is read
    /// into the store at the first fetch.
    fn fetch_inner(&mut self, id: u32, join: &MergeJoin<'_>) -> Result<MergeEval, SqlError> {
        let cx = self.cx;
        let inner_relids = cx.model.plan.path(join.inner).relids;
        self.fill(id, join.inner, |record| merge_record_rows(record, false))?;
        let record = merge_record(self.records, id);
        if !record.inner.arranged {
            if join.inner_sort.as_slice().is_empty() {
                record.inner.arrange_stored()?;
            } else {
                let (members, flags, count) = cx.sort_members(&join.inner_sort, inner_relids);
                let keys = (&members[..count], &flags[..count]);
                record
                    .inner
                    .arrange(cx, keys.0, join.inner, |rows, sizes| {
                        cx.sort_order(join.inner, rows, sizes, keys)
                            .map(|(order, _)| order)
                    })?;
            }
        }
        if record.inner_next < record.inner.len() {
            let at = record.inner_next;
            record.inner_at = Some(at);
            record.inner_next += 1;
            self.bind_inner(id, join, at)?;
            Ok(self.eval(join, false))
        } else {
            record.inner_at = None;
            Ok(MergeEval::EndOfJoin)
        }
    }

    /// Binds the current outer row's relations again.
    fn bind_outer(&mut self, id: u32, join: &MergeJoin<'_>) {
        let relids = self.cx.model.plan.path(join.outer).relids;
        let record = merge_record(self.records, id);
        self.cx.bind(self.bound, record.outer_row, relids);
    }

    /// Binds inner row `at`, decoding it unless it is the one decoded last.
    fn bind_inner(&mut self, id: u32, join: &MergeJoin<'_>, at: usize) -> Result<(), SqlError> {
        let cx = self.cx;
        let relids = cx.model.plan.path(join.inner).relids;
        let record = merge_record(self.records, id);
        if record.inner_loaded != Some(at) {
            Step::release(cx.arena, &mut record.inner_mark);
            record.inner_loaded = None;
            record.inner.load(at, cx, self.bound)?;
            cx.bind(record.inner_row, self.bound, relids);
            record.inner_loaded = Some(at);
        }
        cx.bind(self.bound, record.inner_row, relids);
        Ok(())
    }

    /// `MJEvalOuterValues` or `MJEvalInnerValues` of the bound row: a NULL
    /// first key sorted last ends the join unless the join returns that
    /// side's unmatched rows.
    fn eval(&self, join: &MergeJoin<'_>, outer_side: bool) -> MergeEval {
        let fills = if outer_side {
            join.jointype.fills_outer()
        } else {
            join.jointype.fills_inner()
        };
        let mut result = MergeEval::Matchable;
        for (at, &(o, i)) in JoinContext::keys(&join.keys).enumerate() {
            if self
                .cx
                .value(self.bound, if outer_side { o } else { i })
                .is_null()
            {
                if at == 0 && join.nulls_first & 1 == 0 && !fills {
                    return MergeEval::EndOfJoin;
                }
                result = MergeEval::NonMatchable;
            }
        }
        result
    }

    /// `MJCompare` of the current outer row with the current inner row, or
    /// with inner row `inner`: each key compared as its sort orders it
    /// (`ApplySortComparator`), NULL equal to NULL only to order the rows,
    /// never to join them.
    fn compare(
        &mut self,
        id: u32,
        join: &MergeJoin<'_>,
        inner: Option<usize>,
    ) -> Result<Ordering, SqlError> {
        let cx = self.cx;
        let at = match inner {
            Some(at) => at,
            None => merge_record(self.records, id)
                .inner_at
                .expect("a current inner row"),
        };
        self.bind_outer(id, join);
        self.bind_inner(id, join, at)?;
        let mut null_equals_null = false;
        for (at, &(o, i)) in JoinContext::keys(&join.keys).enumerate() {
            let (one, two) = (cx.value(self.bound, o), cx.value(self.bound, i));
            let first = join.nulls_first >> at & 1 != 0;
            let order = match (one.is_null(), two.is_null()) {
                (true, true) => {
                    null_equals_null = true;
                    continue;
                }
                (true, false) if first => Ordering::Less,
                (true, false) => Ordering::Greater,
                (false, true) if first => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => {
                    let order = crate::sql::eval::compare_datums_collated(
                        cx.storage,
                        cx.txid,
                        cx.collation(o),
                        &one,
                        &two,
                    )?;
                    if join.descending >> at & 1 != 0 {
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
        Ok(if null_equals_null {
            Ordering::Greater
        } else {
            Ordering::Equal
        })
    }

    /// `MJFillOuter`: the current outer row with NULLs for the inner
    /// relations, when the join's filters pass it.
    fn fill_outer(&mut self, id: u32, join: &MergeJoin<'_>) -> Result<bool, SqlError> {
        self.bind_outer(id, join);
        self.cx
            .null_extend(self.bound, self.cx.model.plan.path(join.inner).relids);
        self.cx.equal(self.bound, join.other_quals)
    }

    /// `MJFillInner`: the current inner row with NULLs for the outer
    /// relations, when the join's filters pass it.
    fn fill_inner(&mut self, id: u32, join: &MergeJoin<'_>) -> Result<bool, SqlError> {
        let at = merge_record(self.records, id)
            .inner_at
            .expect("a current inner row");
        self.bind_inner(id, join, at)?;
        self.cx
            .null_extend(self.bound, self.cx.model.plan.path(join.outer).relids);
        self.cx.equal(self.bound, join.other_quals)
    }
}

/// A merge join's sorted outer rows, or its inner rows.
fn merge_record_rows<'r>(record: &'r mut Record<'_>, outer: bool) -> &'r mut OrderedRows {
    let Record::MergeJoin(record) = record else {
        unreachable!("a merge join's record");
    };
    if outer {
        record.outer_sorted.as_mut().expect("a sorted outer input")
    } else {
        &mut record.inner
    }
}
