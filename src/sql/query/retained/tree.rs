//! The retained plan tree: nodes that each produce one row at a time and
//! keep their position between rows, so a wait anywhere returns to the
//! driver with every node's state intact and the same call resumes.
//!
//! A plan is a chain of fixed node records held in the reader itself, inside
//! the stream's or cursor's allocation: a leaf, then each node reading the
//! one before it. A join node holds one scan per FROM item as its levels, in
//! the reader's arena. Sorts and groupings keep their rows in
//! `plan_build_bytes` leases.
//!
//! Every node implements the same steps: [`PlanTree::advance`] moves to the
//! next row (`Ok(false)` at the end), the current row is read through
//! [`PlanTree::with_row`], and [`PlanTree::release`] drops it once the
//! driver has finished with it. A filter, projection or limit at the top of
//! the chain is run by the statement driver itself, so a routine call there
//! hands off on its row; a filter under a sort or grouping runs as that
//! node reads its input.

use super::*;

/// One table position, literal list, generator call or FROM-less row, and
/// its current row.
pub(super) struct ScanNode<'a> {
    pub(super) source: RetainedSource<'a>,
    pub(super) pending: Option<PendingRow<'a>>,
    /// The source columns the query reads; `None` decodes all of them.
    pub(super) decoded_columns: Option<&'a [bool; MAX_COLUMNS]>,
}

impl<'a> ScanNode<'a> {
    /// Reads the next row into `state_arena` unless one is pending.
    /// `Ok(false)` is the end of the source. A failed read keeps the source
    /// position and releases its scratch bytes.
    pub(super) fn read(
        &mut self,
        state_arena: &'a Arena,
        buffers: &mut RowCursorBuffers,
    ) -> Result<bool, SqlError> {
        if self.pending.is_some() {
            return Ok(true);
        }
        let mark = state_arena.mark();
        match next_source_row(&mut self.source, state_arena, buffers, self.decoded_columns) {
            Ok(Some(row)) => {
                self.pending = Some(PendingRow {
                    row,
                    mark,
                    counted: false,
                    saved: None,
                });
                Ok(true)
            }
            result => {
                // SAFETY: nothing borrows the bytes of a row not returned.
                unsafe { state_arena.rewind_to(mark) };
                result.map(|_| false)
            }
        }
    }

    /// Drops the current row and its bytes. The caller has dropped every
    /// evaluation value of the row.
    pub(super) fn release(&mut self, state_arena: &Arena) {
        if let Some(pending) = self.pending.take() {
            // SAFETY: rows nest in the arena; this is the latest one.
            unsafe { state_arena.rewind_to(pending.mark) };
        }
    }

    /// The current row as a one-table joined row.
    pub(super) fn with_row<'work, R>(
        &'work self,
        scope: &'work QueryScope<'a>,
        each: impl FnOnce(&scan::JoinRow<'_, 'work, '_>) -> Result<R, SqlError>,
    ) -> Result<R, SqlError> {
        let pending = self.pending.as_ref().expect("prepared statement row");
        let mut values = [Datum::Null; MAX_COLUMNS];
        pending.decode(scope, &mut values)?;
        let rowids = [match &pending.row {
            RetainedRow::Physical(row) => Some(row.rowid),
            RetainedRow::Saved { rowid, .. } => *rowid,
            RetainedRow::Current { rowid, .. } => Some(*rowid),
            RetainedRow::Singleton | RetainedRow::Literal(_) | RetainedRow::Null => None,
        }];
        each(&scan::JoinRow {
            scope,
            values: scan::JoinRowValues::Flat {
                values: &values,
                offsets: &[0],
            },
            rowids: &rowids,
            visible: scope.n,
        })
    }
}

/// One node of a retained plan.
#[expect(
    clippy::large_enum_variant,
    reason = "nodes live once per reader in its arena; a scan keeps its row cursor inline"
)]
pub(super) enum PlanNode<'a> {
    Scan(ScanNode<'a>),
    /// Nested-loop and hash join levels, each over its own scan.
    Join(join::JoinState<'a>),
    /// A WHERE condition: run by the driver on each returned row when
    /// `by_driver`, else as a sort or grouping reads its input.
    Filter {
        predicate: &'a Expr<'a>,
        by_driver: bool,
    },
    /// Reads its whole input at its first row, then returns it sorted; also
    /// `DISTINCT ON`.
    Sort(sort::SortState<'a>),
    /// `GROUP BY`, aggregates, `HAVING` and plain `DISTINCT`: reads its
    /// whole input at its first row, then returns one row per group.
    Group(grouping::GroupState<'a>),
    /// The output items, evaluated by the driver per returned row.
    Project(&'a [SelectItem<'a>]),
    /// `LIMIT` and `OFFSET`, counted by the driver; `initial` is restored
    /// when a scrollable cursor rewinds.
    Limit {
        bounds: RowBounds<'a>,
        initial: RowBounds<'a>,
    },
}

/// How far a plan's leaf input has been read.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ReaderState {
    Unstarted,
    Reading,
    Complete,
    /// A constant-false filter: the input is never read.
    Eliminated,
}

/// The arguments every node's step reads.
pub(super) struct Pull<'c, 'a> {
    pub(super) scope: &'c QueryScope<'a>,
    pub(super) storage: &'c Storage,
    pub(super) txid: u32,
    pub(super) observed_at: i64,
    pub(super) state_arena: &'a Arena,
    pub(super) buffers: &'c mut RowCursorBuffers,
    pub(super) work: &'c Arena,
    pub(super) params: &'c [Datum<'c>],
    /// The statement's subquery results, run before its first row.
    pub(super) subqueries: Option<&'c crate::sql::eval::SubqueryValues<'c, 'c>>,
}

/// A retained plan: its nodes, leaf first and root last.
pub(super) struct PlanTree<'a> {
    /// The first `count` entries are the chain.
    slots: [PlanNode<'a>; MAX_PLAN_NODES],
    count: usize,
    pub(super) state: ReaderState,
    /// A scrollable cursor keeps its finished input open to read it again.
    pub(super) keep_finished: bool,
}

/// Most nodes in one plan: a leaf, a filter, a sort or grouping, the
/// projection and the limit.
const MAX_PLAN_NODES: usize = 5;

impl<'a> PlanTree<'a> {
    /// Chains `leaf`, the optional `filter`, the optional sort or grouping
    /// `above` it, the projection and the limit.
    pub(super) fn new(
        leaf: PlanNode<'a>,
        filter: Option<&'a Expr<'a>>,
        above: Option<PlanNode<'a>>,
        items: &'a [SelectItem<'a>],
        bounds: RowBounds<'a>,
        eliminated: bool,
    ) -> Result<Self, SqlError> {
        debug_assert!(matches!(leaf, PlanNode::Scan(_) | PlanNode::Join(_)));
        debug_assert!(matches!(
            above,
            None | Some(PlanNode::Sort(_) | PlanNode::Group(_))
        ));
        let by_driver = above.is_none();
        let mut slots: [PlanNode<'a>; MAX_PLAN_NODES] =
            core::array::from_fn(|_| PlanNode::Project(&[]));
        let mut count = 0;
        for node in [
            Some(leaf),
            filter.map(|predicate| PlanNode::Filter {
                predicate,
                by_driver,
            }),
            above,
            Some(PlanNode::Project(items)),
            Some(PlanNode::Limit {
                bounds,
                initial: bounds,
            }),
        ]
        .into_iter()
        .flatten()
        {
            slots[count] = node;
            count += 1;
        }
        Ok(Self {
            slots,
            count,
            state: if eliminated {
                ReaderState::Eliminated
            } else {
                ReaderState::Unstarted
            },
            keep_finished: false,
        })
    }

    fn nodes(&self) -> &[PlanNode<'a>] {
        &self.slots[..self.count]
    }

    fn nodes_mut(&mut self) -> &mut [PlanNode<'a>] {
        &mut self.slots[..self.count]
    }

    pub(super) fn leaf(&self) -> &PlanNode<'a> {
        &self.slots[0]
    }

    pub(super) fn leaf_mut(&mut self) -> &mut PlanNode<'a> {
        &mut self.slots[0]
    }

    /// The single-source scan, when the leaf is not a join.
    pub(super) fn scan(&self) -> Option<&ScanNode<'a>> {
        match self.leaf() {
            PlanNode::Scan(scan) => Some(scan),
            _ => None,
        }
    }

    pub(super) fn scan_mut(&mut self) -> Option<&mut ScanNode<'a>> {
        match self.leaf_mut() {
            PlanNode::Scan(scan) => Some(scan),
            _ => None,
        }
    }

    pub(super) fn join(&self) -> Option<&join::JoinState<'a>> {
        match self.leaf() {
            PlanNode::Join(join) => Some(join),
            _ => None,
        }
    }

    pub(super) fn sort(&self) -> Option<&sort::SortState<'a>> {
        self.nodes().iter().find_map(|node| match node {
            PlanNode::Sort(sort) => Some(sort),
            _ => None,
        })
    }

    pub(super) fn group(&self) -> Option<&grouping::GroupState<'a>> {
        self.nodes().iter().find_map(|node| match node {
            PlanNode::Group(group) => Some(group),
            _ => None,
        })
    }

    pub(super) fn items(&self) -> &'a [SelectItem<'a>] {
        self.nodes()
            .iter()
            .find_map(|node| match node {
                PlanNode::Project(items) => Some(*items),
                _ => None,
            })
            .expect("plan projects its output")
    }

    /// The filter the driver runs on each row: one not read by a sort or
    /// grouping.
    pub(super) fn driver_filter(&self) -> Option<&'a Expr<'a>> {
        self.nodes().iter().find_map(|node| match node {
            PlanNode::Filter {
                predicate,
                by_driver: true,
            } => Some(*predicate),
            _ => None,
        })
    }

    pub(super) fn bounds(&self) -> RowBounds<'a> {
        match self.nodes().last() {
            Some(PlanNode::Limit { bounds, .. }) => *bounds,
            _ => unreachable!("plan root is its limit"),
        }
    }

    pub(super) fn bounds_mut(&mut self) -> &mut RowBounds<'a> {
        match self.nodes_mut().last_mut() {
            Some(PlanNode::Limit { bounds, .. }) => bounds,
            _ => unreachable!("plan root is its limit"),
        }
    }

    /// Whether any node holds a current row.
    pub(super) fn has_current(&self) -> bool {
        self.nodes().iter().any(|node| match node {
            PlanNode::Scan(scan) => scan.pending.is_some(),
            PlanNode::Join(join) => join.has_pending(),
            PlanNode::Sort(sort) => sort.has_current(),
            PlanNode::Group(group) => group.has_current(),
            PlanNode::Filter { .. } | PlanNode::Project(_) | PlanNode::Limit { .. } => false,
        })
    }

    /// Closes the leaf's source after its last row, unless a scrollable
    /// cursor reads it again.
    fn finish_input(&mut self) {
        if !matches!(self.state, ReaderState::Eliminated) {
            self.state = ReaderState::Complete;
        }
        if !self.keep_finished {
            close_leaf(&mut self.slots[0]);
        }
    }

    /// Moves the root to its next row. `Ok(false)` is the end. A wait keeps
    /// every node's position; the same call resumes.
    pub(super) fn advance(&mut self, pull: &mut Pull<'_, 'a>) -> Result<bool, SqlError> {
        if let RowBounds::Active { remaining: 0, .. } = self.bounds() {
            self.finish_input();
            return Ok(false);
        }
        let PlanTree {
            slots,
            count,
            state,
            keep_finished,
        } = self;
        advance(&mut slots[..*count], state, *keep_finished, pull)
    }

    /// The root's current row as a joined row over the scope: a sort's
    /// saved input columns, or the leaf's row.
    pub(super) fn with_row<'work, R>(
        &'work self,
        scope: &'work QueryScope<'a>,
        work: &'work Arena,
        each: impl FnOnce(&scan::JoinRow<'_, 'work, '_>) -> Result<R, SqlError>,
    ) -> Result<R, SqlError> {
        with_row(self.nodes(), scope, work, each)
    }

    /// Drops the root's current row.
    pub(super) fn release(&mut self, state_arena: &Arena) {
        release(self.nodes_mut(), state_arena);
    }

    /// Returns to the first row under the same snapshot; nothing is
    /// allocated. A sort keeps its rows and rereads them.
    pub(super) fn rewind(&mut self, state_arena: &Arena) -> Result<(), SqlError> {
        let PlanTree {
            slots,
            count,
            state,
            ..
        } = self;
        rewind(&mut slots[..*count], state, state_arena)
    }

    /// Starts generator sources: their constant arguments are evaluated
    /// once, when the reader first runs.
    pub(super) fn start(
        &mut self,
        storage: &Storage,
        txid: u32,
        state_arena: &'a Arena,
        params: &[Datum<'a>],
    ) -> Result<(), SqlError> {
        match self.leaf_mut() {
            PlanNode::Scan(scan) => {
                start_generator_source(&mut scan.source, storage, txid, state_arena, params)
            }
            PlanNode::Join(join) => join.start_generators(storage, txid, state_arena, params),
            _ => unreachable!("plan leaf is a scan or join"),
        }
    }

    /// Builds a join's hash tables before its first row.
    pub(super) fn build(&mut self, pull: &mut Pull<'_, 'a>) -> Result<(), SqlError> {
        if matches!(self.state, ReaderState::Eliminated) {
            return Ok(());
        }
        match self.leaf_mut() {
            PlanNode::Join(join) => join.build_hashes(
                pull.scope,
                pull.storage,
                pull.txid,
                pull.observed_at,
                pull.state_arena,
                pull.buffers,
                pull.work,
            ),
            _ => Ok(()),
        }
    }
}

fn close_leaf(node: &mut PlanNode<'_>) {
    match node {
        PlanNode::Scan(scan) => scan.source.close(),
        PlanNode::Join(join) => join.close(),
        _ => unreachable!("plan leaf is a scan or join"),
    }
}

/// Starts reading the leaf; `false` once its input ended or was
/// eliminated.
fn start_leaf(
    state: &mut ReaderState,
    scan: Option<&ScanNode<'_>>,
    pull: &Pull<'_, '_>,
) -> Result<bool, SqlError> {
    match state {
        ReaderState::Complete | ReaderState::Eliminated => Ok(false),
        ReaderState::Unstarted => {
            if scan.is_some_and(|scan| matches!(scan.source, RetainedSource::Physical(_))) {
                pull.storage.record_relation_scan(
                    pull.txid,
                    pull.scope.slots[0],
                    None,
                    0,
                    pull.observed_at,
                )?;
            }
            *state = ReaderState::Reading;
            Ok(true)
        }
        ReaderState::Reading => Ok(true),
    }
}

fn advance<'a>(
    nodes: &mut [PlanNode<'a>],
    state: &mut ReaderState,
    keep_finished: bool,
    pull: &mut Pull<'_, 'a>,
) -> Result<bool, SqlError> {
    let (node, below) = nodes.split_last_mut().expect("plan node");
    match node {
        PlanNode::Scan(scan) => {
            if !start_leaf(state, Some(scan), pull)? {
                return Ok(false);
            }
            check_timeout()?;
            if scan
                .pending
                .as_ref()
                .is_some_and(|pending| pending.saved.is_some())
            {
                return Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "cursor read its source while a saved input was pending"
                ));
            }
            if let RetainedSource::Append {
                branches,
                next,
                targets,
            } = &mut scan.source
            {
                if scan.pending.is_some() {
                    return Ok(true);
                }
                let work = pull.work;
                let mark = work.mark();
                let read = next_appended_row(branches, next, targets, pull, work).and_then(|row| {
                    // The query released its own row: this one starts
                    // where that one began.
                    let start = pull.state_arena.mark();
                    row.map(|bytes| {
                        pull.state_arena
                            .alloc_slice_copy(bytes)
                            .map(|bytes| (start, &*bytes))
                            .map_err(|_| arena_full())
                    })
                    .transpose()
                });
                // SAFETY: the encoded row was copied into the state arena.
                unsafe { work.rewind_to(mark) };
                let Some((start, bytes)) = read? else {
                    *state = ReaderState::Complete;
                    return Ok(false);
                };
                scan.pending = Some(PendingRow {
                    row: RetainedRow::Literal(bytes),
                    mark: start,
                    counted: true,
                    saved: None,
                });
                return Ok(true);
            }
            if !scan.read(pull.state_arena, pull.buffers)? {
                *state = ReaderState::Complete;
                if !keep_finished {
                    scan.source.close();
                }
                return Ok(false);
            }
            let pending = scan.pending.as_mut().expect("captured statement row");
            if !pending.counted {
                if matches!(pending.row, RetainedRow::Physical(_)) {
                    pull.storage.record_relation_tuple_read(
                        pull.txid,
                        pull.scope.slots[0],
                        None,
                    )?;
                }
                pending.counted = true;
            }
            Ok(true)
        }
        PlanNode::Join(join) => {
            if !start_leaf(state, None, pull)? {
                return Ok(false);
            }
            let ready = join.prepare_row(
                pull.scope,
                pull.storage,
                pull.txid,
                pull.observed_at,
                pull.state_arena,
                pull.buffers,
                pull.work,
                pull.params,
                pull.subqueries,
            )?;
            if !ready {
                *state = ReaderState::Complete;
                if !keep_finished {
                    join.close();
                }
            }
            Ok(ready)
        }
        PlanNode::Filter {
            by_driver: true, ..
        } => advance(below, state, keep_finished, pull),
        PlanNode::Filter { predicate, .. } => loop {
            if !advance(below, state, keep_finished, pull)? {
                return Ok(false);
            }
            let work = pull.work;
            let mark = work.mark();
            let passes = with_row(below, pull.scope, work, |row| {
                let catalog = StorageCatalog {
                    storage: pull.storage,
                    routine_workspace: work,
                    txid: pull.txid,
                    invocations: None,
                    statement_arena: None,
                };
                let hooks = EvalHooks {
                    catalog: Some(&catalog),
                    subs: pull.subqueries,
                    ..crate::sql::eval::NO_HOOKS
                };
                where_passes(predicate, work, pull.params, row, &hooks)
            });
            // SAFETY: the evaluation values were dropped with the row.
            unsafe { work.rewind_to(mark) };
            if passes? {
                return Ok(true);
            }
            release(below, pull.state_arena);
        },
        PlanNode::Sort(sort) => {
            while !sort.filled() {
                if !advance(below, state, keep_finished, pull)? {
                    sort.finish_fill(pull.storage, pull.txid)?;
                    break;
                }
                let work = pull.work;
                let mark = work.mark();
                let pushed = with_row(below, pull.scope, work, |row| {
                    sort.encode_input(
                        pull.scope,
                        pull.storage,
                        pull.txid,
                        row,
                        work,
                        pull.params,
                        pull.subqueries,
                    )
                })
                .and_then(|row| sort.push_encoded(row));
                // SAFETY: the encoded row was copied into the sort's lease.
                unsafe { work.rewind_to(mark) };
                pushed?;
                release(below, pull.state_arena);
            }
            sort.advance(pull.storage, pull.txid)
        }
        PlanNode::Group(group) => {
            while group.filling() {
                if !advance(below, state, keep_finished, pull)? {
                    group.finish_input(pull.storage, pull.txid, pull.work)?;
                    break;
                }
                let work = pull.work;
                let mark = work.mark();
                let accepted = with_row(below, pull.scope, work, |row| {
                    let catalog = StorageCatalog {
                        storage: pull.storage,
                        routine_workspace: work,
                        txid: pull.txid,
                        invocations: None,
                        statement_arena: None,
                    };
                    let hooks = EvalHooks {
                        catalog: Some(&catalog),
                        subs: pull.subqueries,
                        ..crate::sql::eval::NO_HOOKS
                    };
                    group.accept(
                        pull.scope,
                        pull.storage,
                        pull.txid,
                        row,
                        work,
                        pull.params,
                        &hooks,
                    )
                });
                // SAFETY: the row's keys, states or saved columns were
                // copied into the grouping's leases.
                unsafe { work.rewind_to(mark) };
                accepted?;
                release(below, pull.state_arena);
            }
            group.fold(pull.scope, pull.storage, pull.txid, pull.work, pull.params)?;
            Ok(group.advance())
        }
        PlanNode::Project(_) | PlanNode::Limit { .. } => advance(below, state, keep_finished, pull),
    }
}

fn with_row<'work, 'a, R>(
    nodes: &'work [PlanNode<'a>],
    scope: &'work QueryScope<'a>,
    work: &'work Arena,
    each: impl FnOnce(&scan::JoinRow<'_, 'work, '_>) -> Result<R, SqlError>,
) -> Result<R, SqlError> {
    let (node, below) = nodes.split_last().expect("plan node");
    match node {
        PlanNode::Scan(scan) => scan.with_row(scope, each),
        PlanNode::Join(join) => join.with_row(scope, work, scope.n, each),
        PlanNode::Sort(sort) if sort.has_current() => sort.with_input_row(scope, work, each),
        PlanNode::Group(_) => Err(sql_err!(
            sqlstate::INTERNAL_ERROR,
            "a grouped row has no input row"
        )),
        PlanNode::Sort(_)
        | PlanNode::Filter { .. }
        | PlanNode::Project(_)
        | PlanNode::Limit { .. } => with_row(below, scope, work, each),
    }
}

fn release(nodes: &mut [PlanNode<'_>], state_arena: &Arena) {
    let (node, below) = nodes.split_last_mut().expect("plan node");
    match node {
        PlanNode::Scan(scan) => scan.release(state_arena),
        PlanNode::Join(join) => join.finish_row(state_arena),
        PlanNode::Sort(sort) => sort.finish_row(),
        PlanNode::Group(group) => group.finish_row(),
        PlanNode::Filter { .. } | PlanNode::Project(_) | PlanNode::Limit { .. } => {
            release(below, state_arena)
        }
    }
}

fn rewind(
    nodes: &mut [PlanNode<'_>],
    state: &mut ReaderState,
    state_arena: &Arena,
) -> Result<(), SqlError> {
    let (node, below) = nodes.split_last_mut().expect("plan node");
    match node {
        PlanNode::Limit { bounds, initial } => {
            rewind(below, state, state_arena)?;
            *bounds = *initial;
            Ok(())
        }
        PlanNode::Filter { .. } | PlanNode::Project(_) => rewind(below, state, state_arena),
        // The sorted rows stay; only the output position returns.
        PlanNode::Sort(sort) => {
            sort.rewind();
            Ok(())
        }
        PlanNode::Group(_) => Err(sql_err!(
            sqlstate::INTERNAL_ERROR,
            "a retained grouping cannot be rewound"
        )),
        PlanNode::Scan(scan) => {
            scan.source.rewind(state_arena)?;
            // PostgreSQL counts a scrollable scan once, however it moves;
            // the rows read again are counted as read.
            *state = ReaderState::Reading;
            Ok(())
        }
        PlanNode::Join(join) => {
            join.rewind(state_arena)?;
            *state = ReaderState::Reading;
            Ok(())
        }
    }
}
