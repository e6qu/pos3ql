//! Per-node counters for `EXPLAIN ANALYZE`.
//!
//! EXPLAIN builds its plan from the planning decisions execution makes, and
//! names each node it can measure by a [`NodeKey`]: the query block the node
//! belongs to and its role in that block (the scan of a FROM position, the
//! join that adds a position, the block's sort, ...). Both sides derive a
//! block's identity the same way, from its place in the statement: the
//! derived table at a FROM position, the leaf of a set operation, the source
//! query of a write.
//!
//! While an analyzed statement runs, [`analyze`] installs the plan's counter
//! slots for the current thread. Each operator asks for its node with
//! [`node`], opens one loop per execution ([`Node::start`]) and counts the
//! rows it returns and the rows its qualifications remove. The slots live in
//! the statement arena next to the plan, so counting allocates nothing; an
//! attempt that restarts after a wait plans, and so counts, afresh.
//!
//! Routine and trigger bodies run statements of their own, which are not part
//! of the explained plan: [`suspend`] hides the counters while they run.

use core::cell::Cell;
use core::ptr::NonNull;

/// A query block's identity within one statement.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct BlockId(u64);

/// Where a nested query block sits in its parent.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Segment {
    /// The query of the derived table at this FROM position.
    Derived(u16),
    /// The leaf of a set operation at this left-to-right position.
    Branch(u16),
    /// The query a write reads its rows from (`INSERT ... SELECT`).
    Source,
    /// A subquery in an expression. EXPLAIN does not show these yet; the
    /// segment keeps their operators apart from the enclosing block's.
    Sublink,
    /// A `WITH` query run once. EXPLAIN does not show these yet; the
    /// segment keeps their operators apart.
    Cte,
}

impl Segment {
    const fn code(self) -> u64 {
        match self {
            Self::Derived(position) => (1 << 32) | position as u64,
            Self::Branch(position) => (2 << 32) | position as u64,
            Self::Source => 3 << 32,
            Self::Sublink => 4 << 32,
            Self::Cte => 5 << 32,
        }
    }
}

const fn mix(mut value: u64) -> u64 {
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

impl BlockId {
    /// The statement's top query block.
    pub(crate) const ROOT: Self = Self(0x9e37_79b9_7f4a_7c15);

    /// The block nested in this one at `segment`.
    pub(crate) const fn child(self, segment: Segment) -> Self {
        Self(mix(self.0 ^ mix(segment.code())))
    }
}

/// A node's role in its query block.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Role {
    /// The scan of the FROM position.
    Scan(u16),
    /// The join adding the relation at this join-order depth (from 1).
    Join(u16),
    /// The hash table built from the FROM position.
    Hash(u16),
    /// The materialized copy of the FROM position a nested loop rescans.
    Materialize(u16),
    /// The block's aggregation or grouping.
    Aggregate,
    /// The block's window evaluation.
    Window,
    /// The block's `ORDER BY` sort.
    Sort,
    /// The block's `DISTINCT`.
    Unique,
    /// The block's `LIMIT` or `OFFSET`.
    Limit,
    /// A block without relations: one computed row.
    Result,
    /// The set operation at this pre-order position of its tree.
    SetOperation(u16),
    /// The `Append` of a hashed set operation's inputs.
    SetInput(u16),
    /// A write's target.
    Modify,
}

/// A plan node's identity: its block and its role there.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct NodeKey {
    pub(crate) block: BlockId,
    pub(crate) role: Role,
}

impl NodeKey {
    pub(crate) const fn new(block: BlockId, role: Role) -> Self {
        Self { block, role }
    }
}

/// What one node did while the statement ran, summed over its loops.
#[derive(Clone, Copy, Default)]
pub(crate) struct NodeCounters {
    pub(crate) loops: u64,
    pub(crate) rows: u64,
    /// Rows its `Filter` removed.
    pub(crate) filtered: u64,
    /// Rows its `Join Filter` removed.
    pub(crate) join_filtered: u64,
    /// Time from each loop's start to its first row, or to its end when it
    /// returned none.
    pub(crate) startup_nanos: u64,
    pub(crate) total_nanos: u64,
}

/// One plan node's counters while the statement runs.
#[derive(Clone, Copy)]
pub(crate) struct Slot {
    key: Option<NodeKey>,
    counters: NodeCounters,
    /// Nested starts of the open loop; only the outermost one counts.
    open: u32,
    started: Option<std::time::Instant>,
    first_row: Option<u64>,
}

impl Slot {
    pub(crate) const fn new(key: Option<NodeKey>) -> Self {
        Self {
            key,
            counters: NodeCounters {
                loops: 0,
                rows: 0,
                filtered: 0,
                join_filtered: 0,
                startup_nanos: 0,
                total_nanos: 0,
            },
            open: 0,
            started: None,
            first_row: None,
        }
    }

    pub(crate) const fn counters(&self) -> NodeCounters {
        self.counters
    }
}

struct Registry {
    slots: NonNull<[Slot]>,
    timing: bool,
}

thread_local! {
    static ACTIVE: Cell<Option<NonNull<Registry>>> = const { Cell::new(None) };
    static BLOCK: Cell<BlockId> = const { Cell::new(BlockId::ROOT) };
}

struct Restore {
    active: Option<NonNull<Registry>>,
    block: BlockId,
}

impl Drop for Restore {
    fn drop(&mut self) {
        ACTIVE.with(|active| active.set(self.active));
        BLOCK.with(|block| block.set(self.block));
    }
}

/// Runs `execute` with `slots` counting its operators, starting at the top
/// block. `timing` measures each loop's time as well.
pub(crate) fn analyze<R>(slots: &mut [Slot], timing: bool, execute: impl FnOnce() -> R) -> R {
    let mut registry = Registry {
        slots: NonNull::from(slots),
        timing,
    };
    let _restore = Restore {
        active: ACTIVE.with(|active| active.replace(Some(NonNull::from(&mut registry)))),
        block: BLOCK.with(|block| block.replace(BlockId::ROOT)),
    };
    execute()
}

/// Hides the counters from statements a routine or trigger body runs, until
/// the returned scope ends.
#[must_use]
pub(crate) fn suspend() -> Suspended {
    Suspended(Restore {
        active: ACTIVE.with(|active| active.replace(None)),
        block: BLOCK.with(|block| block.replace(BlockId::ROOT)),
    })
}

pub(crate) struct Suspended(#[allow(dead_code)] Restore);

/// Whether an analyzed statement is counting on this thread.
pub(crate) fn active() -> bool {
    ACTIVE.with(Cell::get).is_some()
}

/// The block the executor is running.
#[cfg(test)]
fn current_block() -> BlockId {
    BLOCK.with(Cell::get)
}

/// Runs nested blocks at `segment` of the current one until the returned
/// scope ends.
#[must_use]
pub(crate) fn enter(segment: Segment) -> Entered {
    let parent = BLOCK.with(Cell::get);
    BLOCK.with(|block| block.set(parent.child(segment)));
    Entered(parent)
}

pub(crate) struct Entered(BlockId);

impl Drop for Entered {
    fn drop(&mut self) {
        BLOCK.with(|block| block.set(self.0));
    }
}

/// The node with `role` in the current block, if the explained plan has it.
pub(crate) fn node(role: Role) -> Node {
    let Some(registry) = ACTIVE.with(Cell::get) else {
        return Node::NONE;
    };
    let key = NodeKey::new(BLOCK.with(Cell::get), role);
    // SAFETY: `analyze` keeps the registry and its slots alive and
    // unaliased while it is installed, and `Node`s do not outlive the
    // execution it runs.
    let registry = unsafe { registry.as_ref() };
    let slots = unsafe { &mut *registry.slots.as_ptr() };
    let slot = slots.iter_mut().find(|slot| slot.key == Some(key));
    Node {
        slot: slot.map(NonNull::from),
        timing: registry.timing,
    }
}

/// An operator's counters, or nothing outside `EXPLAIN ANALYZE`.
#[derive(Clone, Copy)]
pub(crate) struct Node {
    slot: Option<NonNull<Slot>>,
    timing: bool,
}

impl Node {
    pub(crate) const NONE: Self = Self {
        slot: None,
        timing: false,
    };

    fn with(self, update: impl FnOnce(&mut Slot, bool)) {
        if let Some(mut slot) = self.slot {
            // SAFETY: see `node`; the executor is single-threaded and no
            // reference into a slot is held across calls.
            update(unsafe { slot.as_mut() }, self.timing);
        }
    }

    /// Whether the plan has this node.
    pub(crate) const fn measured(self) -> bool {
        self.slot.is_some()
    }

    /// Opens one execution of the node, which ends when the returned loop
    /// is dropped. A start inside an open loop of the same node joins it.
    pub(crate) fn start(self) -> Loop {
        self.begin();
        Loop(self)
    }

    fn begin(self) {
        self.with(|slot, timing| {
            slot.open += 1;
            if slot.open == 1 {
                slot.started = timing.then(std::time::Instant::now);
                slot.first_row = None;
            }
        });
    }

    /// The loops the node has finished.
    pub(crate) fn loops(self) -> u64 {
        let mut loops = 0;
        self.with(|slot, _| loops = slot.counters.loops);
        loops
    }

    /// Counts one row the node returned.
    pub(crate) fn row(self) {
        self.rows(1);
    }

    /// Counts `count` rows the node returned.
    pub(crate) fn rows(self, count: u64) {
        if count == 0 {
            return;
        }
        self.with(|slot, _| {
            slot.counters.rows = slot.counters.rows.saturating_add(count);
            if slot.first_row.is_none()
                && let Some(started) = slot.started
            {
                slot.first_row = Some(elapsed_nanos(started));
            }
        });
    }

    /// Counts a row the node's `Filter` removed.
    pub(crate) fn filtered(self) {
        self.with(|slot, _| slot.counters.filtered = slot.counters.filtered.saturating_add(1));
    }

    /// Counts a row the node's `Join Filter` removed.
    pub(crate) fn join_filtered(self) {
        self.with(|slot, _| {
            slot.counters.join_filtered = slot.counters.join_filtered.saturating_add(1);
        });
    }

    /// Moves a counted row to the rows the node's `Filter` removed: a
    /// condition the node's consumer checks for it.
    pub(crate) fn refiltered(self) {
        self.with(|slot, _| {
            slot.counters.rows = slot.counters.rows.saturating_sub(1);
            slot.counters.filtered = slot.counters.filtered.saturating_add(1);
        });
    }

    /// [`Node::refiltered`] for a join's `Join Filter`.
    pub(crate) fn join_refiltered(self) {
        self.with(|slot, _| {
            slot.counters.rows = slot.counters.rows.saturating_sub(1);
            slot.counters.join_filtered = slot.counters.join_filtered.saturating_add(1);
        });
    }

    fn end(self) {
        self.with(|slot, _| {
            slot.open = slot.open.saturating_sub(1);
            if slot.open != 0 {
                return;
            }
            slot.counters.loops += 1;
            if let Some(started) = slot.started.take() {
                let total = elapsed_nanos(started);
                slot.counters.total_nanos = slot.counters.total_nanos.saturating_add(total);
                slot.counters.startup_nanos = slot
                    .counters
                    .startup_nanos
                    .saturating_add(slot.first_row.take().unwrap_or(total));
            }
        });
    }
}

/// The scan or join whose rows a block's scan returns, which also filters
/// by the conditions the block checks on them afterwards: the relation's
/// scan, or the top join of `relations` relations.
#[derive(Clone, Copy)]
pub(crate) struct ScanOutput {
    node: Node,
    join: bool,
}

impl ScanOutput {
    pub(crate) fn current(relations: usize) -> Self {
        if relations <= 1 {
            Self {
                node: node(Role::Scan(0)),
                join: false,
            }
        } else {
            Self {
                node: node(Role::Join(u16::try_from(relations - 1).unwrap_or(u16::MAX))),
                join: true,
            }
        }
    }

    /// Moves one returned row to the rows the node's filter removed.
    pub(crate) fn refiltered(self) {
        if self.join {
            self.node.join_refiltered();
        } else {
            self.node.refiltered();
        }
    }
}

fn elapsed_nanos(started: std::time::Instant) -> u64 {
    started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64
}

/// One open execution of a node.
#[must_use]
pub(crate) struct Loop(Node);

impl Drop for Loop {
    fn drop(&mut self) {
        self.0.end();
    }
}

/// The nodes of a query block's operators above its scans and joins, in
/// PostgreSQL's order from the top: limit, distinct, sort, window,
/// aggregation.
#[derive(Clone, Copy)]
pub(crate) struct Stages {
    pub(crate) limit: Node,
    pub(crate) unique: Node,
    pub(crate) sort: Node,
    pub(crate) window: Node,
    pub(crate) aggregate: Node,
}

impl Stages {
    /// The stages of the current block that the explained plan has.
    pub(crate) fn current() -> Self {
        if !active() {
            return Self {
                limit: Node::NONE,
                unique: Node::NONE,
                sort: Node::NONE,
                window: Node::NONE,
                aggregate: Node::NONE,
            };
        }
        Self {
            limit: node(Role::Limit),
            unique: node(Role::Unique),
            sort: node(Role::Sort),
            window: node(Role::Window),
            aggregate: node(Role::Aggregate),
        }
    }

    /// The stage the limit reads its rows from.
    pub(crate) fn below_limit(&self) -> Node {
        [self.unique, self.sort, self.window, self.aggregate]
            .into_iter()
            .find(|node| node.measured())
            .unwrap_or(Node::NONE)
    }

    /// Opens one execution of every stage.
    pub(crate) fn start(&self) -> StageLoops {
        let nodes = [
            self.limit,
            self.unique,
            self.sort,
            self.window,
            self.aggregate,
        ];
        for node in nodes {
            node.begin();
        }
        StageLoops(nodes)
    }
}

/// One open execution of a block's stages.
#[must_use]
pub(crate) struct StageLoops([Node; 5]);

impl Drop for StageLoops {
    fn drop(&mut self) {
        for node in self.0 {
            node.end();
        }
    }
}

/// One open execution of each of several nodes.
#[must_use]
pub(crate) struct Loops<'n>(&'n [Node]);

impl<'n> Loops<'n> {
    pub(crate) fn start(nodes: &'n [Node]) -> Self {
        for node in nodes {
            node.begin();
        }
        Self(nodes)
    }
}

impl Drop for Loops<'_> {
    fn drop(&mut self) {
        for node in self.0 {
            node.end();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_reach_only_the_named_node_of_the_current_block() {
        let scan = NodeKey::new(BlockId::ROOT, Role::Scan(0));
        let nested = NodeKey::new(BlockId::ROOT.child(Segment::Derived(1)), Role::Scan(0));
        let mut slots = [
            Slot::new(Some(scan)),
            Slot::new(Some(nested)),
            Slot::new(None),
        ];
        analyze(&mut slots, false, || {
            let scan = node(Role::Scan(0));
            for _ in 0..2 {
                let _loop = scan.start();
                scan.row();
                scan.filtered();
            }
            let _derived = enter(Segment::Derived(1));
            let inner = node(Role::Scan(0));
            let _loop = inner.start();
            inner.rows(3);
            assert!(!super::node(Role::Sort).measured());
        });
        assert!(!active());
        assert_eq!(current_block(), BlockId::ROOT);
        let outer = slots[0].counters();
        assert_eq!((outer.loops, outer.rows, outer.filtered), (2, 2, 2));
        let inner = slots[1].counters();
        assert_eq!((inner.loops, inner.rows), (1, 3));
        assert_eq!(slots[2].counters().loops, 0);
    }

    #[test]
    fn suspended_bodies_count_nothing() {
        let scan = NodeKey::new(BlockId::ROOT, Role::Scan(0));
        let mut slots = [Slot::new(Some(scan))];
        analyze(&mut slots, true, || {
            let _body = suspend();
            let scan = node(Role::Scan(0));
            let _loop = scan.start();
            scan.row();
        });
        assert_eq!(slots[0].counters().loops, 0);
    }
}
