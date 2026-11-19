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
//! the statement arena next to the plan, so counting allocates nothing. A
//! statement that runs again from its first row after an attempt stopped
//! counts each node in the last attempt that ran it: a node's counters
//! restart when a later attempt first reaches it, so a node an earlier
//! attempt finished for good (a data-modifying `WITH` query whose write the
//! later attempts keep) reports that attempt's work.
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
    /// The subquery whose query is at this address, run as a subplan: its
    /// operators count apart from the enclosing block's.
    Sublink(u64),
    /// The `WITH` query at this position of its list, run once: its
    /// statement's `CTE name` InitPlan.
    Cte(usize),
    /// The query at this position of the queries rewrite rules turn the
    /// statement into, which the top block holds.
    Query(u16),
}

impl Segment {
    const fn code(self) -> u64 {
        match self {
            Self::Derived(position) => (1 << 32) | position as u64,
            Self::Branch(position) => (2 << 32) | position as u64,
            Self::Source => 3 << 32,
            Self::Sublink(query) => (4 << 56) ^ query,
            // A list position is far below 2^56 entries.
            Self::Cte(position) => (5 << 56) ^ position as u64,
            Self::Query(position) => (6 << 32) | position as u64,
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
    /// The scan of the FROM position: of its relation, or the `Append` of
    /// the members of its partitioned table or inheritance tree.
    Scan(u16),
    /// The `Bitmap Index Scan` under the FROM position's bitmap heap scan.
    BitmapIndex(u16),
    /// The node at this position of the bitmap tree (`BitmapAnd`,
    /// `BitmapOr` and their bitmap index scans) under the bitmap heap scan
    /// of a query's one relation.
    BitmapNode(u16),
    /// The scan of one member of the partitioned table or inheritance tree
    /// at the FROM position, by the member's storage slot.
    Member(u16, u32),
    /// The sort an `Append` or `Merge Append` puts over that member's scan.
    MemberSort(u16, u32),
    /// The `Bitmap Index Scan` under that member's bitmap heap scan.
    MemberBitmapIndex(u16, u32),
    /// The node at this position of that member's bitmap tree.
    MemberBitmapNode(u16, u32, u16),
    /// The join adding the relation at this join-order depth (from 1).
    Join(u16),
    /// The hash table built from the FROM position.
    Hash(u16),
    /// The materialized copy of the FROM position a nested loop rescans.
    Materialize(u16),
    /// The cache over a parameterized scan a nested loop reads.
    Memoize(u16),
    /// The sort of a merge join's outer (0) or inner (1) input.
    MergeSort(u16, u8),
    /// The block's aggregation or grouping.
    Aggregate,
    /// The sort of the input a `GroupAggregate` groups.
    GroupSort,
    /// The block's window evaluation: its last window clause's.
    Window,
    /// The evaluation of the window clause at this position of the
    /// block's evaluation order, below the last.
    WindowAgg(u16),
    /// The sort of the input of the window clause at this position.
    WindowSort(u16),
    /// The block's set-returning functions in its SELECT list.
    ProjectSet,
    /// The block's `ORDER BY` sort.
    Sort,
    /// The block's `DISTINCT`.
    Unique,
    /// The block's `LIMIT` or `OFFSET`.
    Limit,
    /// The block's row locks (`FOR UPDATE`, `FOR SHARE`, ...).
    LockRows,
    /// A block without relations: one computed row.
    Result,
    /// The projection PostgreSQL runs above a block's sort to evaluate its
    /// volatile or expensive select-list expressions on sorted rows. It
    /// reports its sort's figures.
    SortedProjection,
    /// The set operation at this pre-order position of its tree.
    SetOperation(u16),
    /// The `Append` of a hashed set operation's inputs.
    SetInput(u16),
    /// A write's target.
    Modify,
    /// The `VALUES` rows an `INSERT` writes.
    Values,
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
    /// A hash node's table, when it built one.
    pub(crate) hash: Option<HashTable>,
    /// A sort node's method and space, when it sorted.
    pub(crate) sort: Option<SortSpace>,
    /// The most bytes of rows a window node held in memory.
    pub(crate) stored: Option<usize>,
    /// The rows went to a temporary file (`usedDisk`), so the node reports
    /// its storage as disk.
    pub(crate) stored_disk: bool,
    /// A hashed aggregate's peak memory (`hash_mem_peak`).
    pub(crate) hash_agg_peak: Option<usize>,
    /// For the first node of a `WITH` query's block, the most bytes its
    /// tuplestore held, which each `CTE Scan` reading it reports.
    pub(crate) cte_stored: Option<StoredSpace>,
    /// An incremental sort's batches, when it sorted.
    pub(crate) incremental: Option<IncrementalGroups>,
    /// The rows each kind of a `MERGE`'s actions changed.
    pub(crate) merged: MergeTuples,
    /// A bitmap heap scan's distinct heap pages (`Heap Blocks: exact`).
    pub(crate) heap_blocks: u64,
    /// An index-only scan's rows read from pages not all-visible.
    pub(crate) heap_fetches: u64,
    /// A `Memoize` node's cache figures.
    pub(crate) memoize: Option<MemoizeStats>,
    /// The pages the node itself read and changed, without its inputs'.
    pub(crate) buffers: BufferUsage,
    /// A page model counted the node's pages.
    pub(crate) paged: bool,
}

/// Page accesses in PostgreSQL's `BufferUsage` terms, each page the 8 kB
/// heap or index page the planner's page counts model: `shared` for
/// permanent relations, `local` for temporary ones, `temp` for the work
/// files of sorts and tuplestores that outgrow memory.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(crate) struct BufferUsage {
    pub(crate) shared_hit: u64,
    pub(crate) shared_read: u64,
    pub(crate) shared_dirtied: u64,
    pub(crate) shared_written: u64,
    pub(crate) local_hit: u64,
    pub(crate) local_read: u64,
    pub(crate) local_dirtied: u64,
    pub(crate) local_written: u64,
    pub(crate) temp_read: u64,
    pub(crate) temp_written: u64,
}

impl BufferUsage {
    pub(crate) const fn plus(self, other: Self) -> Self {
        Self {
            shared_hit: self.shared_hit.saturating_add(other.shared_hit),
            shared_read: self.shared_read.saturating_add(other.shared_read),
            shared_dirtied: self.shared_dirtied.saturating_add(other.shared_dirtied),
            shared_written: self.shared_written.saturating_add(other.shared_written),
            local_hit: self.local_hit.saturating_add(other.local_hit),
            local_read: self.local_read.saturating_add(other.local_read),
            local_dirtied: self.local_dirtied.saturating_add(other.local_dirtied),
            local_written: self.local_written.saturating_add(other.local_written),
            temp_read: self.temp_read.saturating_add(other.temp_read),
            temp_written: self.temp_written.saturating_add(other.temp_written),
        }
    }

    /// `pages` page accesses of a permanent (`local` false) or temporary
    /// relation, of which `read` needed storage the RAM cache did not hold.
    pub(crate) const fn accessed(local: bool, pages: u64, read: u64) -> Self {
        let hit = pages.saturating_sub(read);
        let mut usage = Self {
            shared_hit: 0,
            shared_read: 0,
            shared_dirtied: 0,
            shared_written: 0,
            local_hit: 0,
            local_read: 0,
            local_dirtied: 0,
            local_written: 0,
            temp_read: 0,
            temp_written: 0,
        };
        if local {
            usage.local_hit = hit;
            usage.local_read = read;
        } else {
            usage.shared_hit = hit;
            usage.shared_read = read;
        }
        usage
    }

    pub(crate) const fn is_zero(&self) -> bool {
        self.shared_hit == 0
            && self.shared_read == 0
            && self.shared_dirtied == 0
            && self.shared_written == 0
            && self.local_hit == 0
            && self.local_read == 0
            && self.local_dirtied == 0
            && self.local_written == 0
            && self.temp_read == 0
            && self.temp_written == 0
    }
}

/// How a scan's page accesses are counted: the relation's modeled heap
/// pages and, for an index search, its index's pages.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PageModel {
    /// A temporary relation, whose pages are local.
    pub(crate) local: bool,
    /// The relation's heap pages and visible rows.
    pub(crate) relation_pages: u64,
    pub(crate) rows: u64,
    /// The index the node searches each loop, if any.
    pub(crate) index: Option<super::index_paths::IndexPages>,
    /// A sequential scan, whose read stream holds the page after the one
    /// it stopped on (`read_stream_next_buffer`).
    pub(crate) sequential: bool,
    /// An index-only scan reading the relation's visibility map, which
    /// pins its one page for the node's whole run.
    pub(crate) visibility_map: bool,
}

/// The current loop's page accesses of a node with a [`PageModel`].
#[derive(Clone, Copy, Default)]
struct PageLoop {
    /// The heap page the scan last read, which reading again costs nothing.
    last: Option<u64>,
    heap: u64,
    /// Of `heap`, the pages whose reaching took a block the RAM cache did
    /// not hold.
    read: u64,
    /// Rows a sequential scan examined.
    examined: u64,
    /// Index entries the search reached.
    entries: u64,
    /// The thread's RAM misses when the last page was counted.
    misses: u64,
}

/// A `Memoize` node's cache figures (`MemoizeInstrumentation`).
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(crate) struct MemoizeStats {
    pub(crate) hits: u64,
    pub(crate) misses: u64,
    pub(crate) evictions: u64,
    pub(crate) overflows: u64,
    /// `mem_peak`: the most bytes its entries and cached rows held.
    pub(crate) peak: usize,
}

/// The rows a `MERGE`'s actions inserted, updated and deleted.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(crate) struct MergeTuples {
    pub(crate) inserted: u64,
    pub(crate) updated: u64,
    pub(crate) deleted: u64,
}

/// The kind of row change a `MERGE` action made.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum MergeChange {
    Insert,
    Update,
    Delete,
}

/// The batches of one kind an incremental sort sorted
/// (`IncrementalSortGroupInfo`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct SortGroups {
    pub(crate) count: u64,
    /// The kilobytes each group reported in memory, summed, and their peak
    /// (`totalMemorySpaceUsed`, `maxMemorySpaceUsed`).
    pub(crate) total_kb: u64,
    pub(crate) peak_kb: u64,
    /// The kilobytes the groups that reported disk space used, summed, and
    /// their peak (`totalDiskSpaceUsed`, `maxDiskSpaceUsed`).
    pub(crate) total_disk_kb: u64,
    pub(crate) peak_disk_kb: u64,
    /// The sort methods the groups used (`sortMethods`).
    pub(crate) top_n: bool,
    pub(crate) quicksort: bool,
    pub(crate) external_merge: bool,
    /// What the batches' one tuplesort remembers across `tuplesort_reset`:
    /// the most space it held so far, whether that was disk, and the method
    /// it sorted by then (`tuplesort_updatemax`), which each group reports.
    max_space: usize,
    max_disk: bool,
    max_method: SortMethod,
    /// The tuplesort kept its array at the initial size after the last
    /// batch, so the next one starts on it uncharged.
    pub(crate) array_kept: bool,
}

impl Default for SortGroups {
    fn default() -> Self {
        Self::NONE
    }
}

impl SortGroups {
    pub(crate) const NONE: Self = Self {
        count: 0,
        total_kb: 0,
        peak_kb: 0,
        total_disk_kb: 0,
        peak_disk_kb: 0,
        top_n: false,
        quicksort: false,
        external_merge: false,
        max_space: 0,
        max_disk: false,
        max_method: SortMethod::Quicksort,
        array_kept: false,
    };

    /// `instrumentSortedGroup` after a batch whose sort used `space`.
    pub(crate) fn add_sorted(&mut self, space: SortSpace) {
        let disk = space.method == SortMethod::ExternalMerge;
        if (disk && !self.max_disk) || (disk == self.max_disk && space.bytes > self.max_space) {
            self.max_space = space.bytes;
            self.max_disk = disk;
            self.max_method = space.method;
        }
        let kb = self.max_space.div_ceil(1024) as u64;
        self.count += 1;
        if self.max_disk {
            self.total_disk_kb += kb;
            self.peak_disk_kb = self.peak_disk_kb.max(kb);
        } else {
            self.total_kb += kb;
            self.peak_kb = self.peak_kb.max(kb);
        }
        match self.max_method {
            SortMethod::TopNHeapsort => self.top_n = true,
            SortMethod::Quicksort => self.quicksort = true,
            SortMethod::ExternalMerge => self.external_merge = true,
        }
    }

    /// The methods' names in `show_incremental_sort_group_info`'s order.
    pub(crate) fn methods(&self) -> impl Iterator<Item = &'static str> {
        [
            (self.top_n, SortMethod::TopNHeapsort),
            (self.quicksort, SortMethod::Quicksort),
            (self.external_merge, SortMethod::ExternalMerge),
        ]
        .into_iter()
        .filter_map(|(used, method)| used.then_some(method.name()))
    }

    /// The average kilobytes a group reported in memory.
    pub(crate) fn average_kb(&self) -> u64 {
        self.total_kb / self.count.max(1)
    }

    /// The average kilobytes a group reported on disk.
    pub(crate) fn average_disk_kb(&self) -> u64 {
        self.total_disk_kb / self.count.max(1)
    }
}

/// An incremental sort's full-sort and presorted-prefix batches.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(crate) struct IncrementalGroups {
    pub(crate) full: SortGroups,
    pub(crate) presorted: SortGroups,
}

/// PostgreSQL's incremental sort (`nodeIncrementalSort.c`) batching, over
/// rows that arrive ordered by the presorted keys, with no bound: it
/// collects at least 32 rows and then the rest of the last one's prefix
/// group into a full sort, and once a full sort passes 64 rows sorts each
/// further prefix group on its own (`switchToPresortedPrefixMode`).
pub(crate) struct IncrementalSortBatches {
    /// The prefix group of the latest row.
    group: u64,
    started: bool,
    state: BatchState,
    /// The full sort being loaded: each row's prefix group, from the input
    /// row `batch_start`.
    batch_groups: [u64; FULL_SORT_LIMIT],
    batch_start: usize,
    batch_rows: usize,
}

/// Which of an incremental sort's two tuplesorts a batch went through.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum BatchKind {
    Full,
    Presorted,
}

const MIN_GROUP: usize = 32;
/// A full sort past `2 * MIN_GROUP` rows switches to presorted mode.
const FULL_SORT_LIMIT: usize = 2 * MIN_GROUP + 1;

#[derive(Clone, Copy)]
enum BatchState {
    /// Loading a full sort; past `MIN_GROUP` rows it takes only rows of
    /// the prefix group of its `MIN_GROUP`th row.
    Full { pivot: Option<u64> },
    /// Loading the rest of prefix group `group`, from input row `start`,
    /// into a prefix sort.
    Prefix { group: u64, start: usize },
}

/// Receives each batch as the input rows it sorted.
pub(crate) type BatchSorted<'d> =
    &'d mut dyn FnMut(BatchKind, core::ops::Range<usize>) -> Result<(), crate::sql::SqlError>;

impl IncrementalSortBatches {
    pub(crate) const fn new() -> Self {
        Self {
            group: 0,
            started: false,
            state: BatchState::Full { pivot: None },
            batch_groups: [0; FULL_SORT_LIMIT],
            batch_start: 0,
            batch_rows: 0,
        }
    }

    /// The input row at position `at`: whether its presorted keys differ
    /// from the previous row's. `sorted` receives each batch it completes.
    pub(crate) fn push(
        &mut self,
        new_group: bool,
        at: usize,
        sorted: BatchSorted<'_>,
    ) -> Result<(), crate::sql::SqlError> {
        if new_group && self.started {
            self.group += 1;
        }
        self.started = true;
        let group = self.group;
        match self.state {
            BatchState::Prefix { group: held, start } => {
                if group == held {
                    return Ok(());
                }
                sorted(BatchKind::Presorted, start..at)?;
                self.start_full(group, at);
            }
            BatchState::Full { pivot } => {
                if self.batch_rows >= MIN_GROUP && pivot.is_some_and(|pivot| pivot != group) {
                    self.finish_full(sorted)?;
                    self.start_full(group, at);
                    return Ok(());
                }
                self.batch_groups[self.batch_rows] = group;
                self.batch_rows += 1;
                if self.batch_rows == MIN_GROUP {
                    self.state = BatchState::Full { pivot: Some(group) };
                }
                if self.batch_rows == FULL_SORT_LIMIT {
                    self.finish_full(sorted)?;
                    self.switch_to_prefix(sorted)?;
                }
            }
        }
        Ok(())
    }

    /// Completes the batch still loading, once `rows` rows were pushed.
    pub(crate) fn finish(
        mut self,
        rows: usize,
        sorted: BatchSorted<'_>,
    ) -> Result<(), crate::sql::SqlError> {
        match self.state {
            BatchState::Full { .. } => self.finish_full(sorted),
            BatchState::Prefix { start, .. } => sorted(BatchKind::Presorted, start..rows),
        }
    }

    fn start_full(&mut self, group: u64, at: usize) {
        self.batch_groups[0] = group;
        self.batch_start = at;
        self.batch_rows = 1;
        self.state = BatchState::Full { pivot: None };
    }

    fn finish_full(&mut self, sorted: BatchSorted<'_>) -> Result<(), crate::sql::SqlError> {
        sorted(
            BatchKind::Full,
            self.batch_start..self.batch_start + self.batch_rows,
        )
    }

    /// Sorts each prefix group of the full sort on its own; the last one
    /// continues in the input.
    fn switch_to_prefix(&mut self, sorted: BatchSorted<'_>) -> Result<(), crate::sql::SqlError> {
        let mut at = 0usize;
        loop {
            let group = self.batch_groups[at];
            let start = self.batch_start + at;
            while at < self.batch_rows && self.batch_groups[at] == group {
                at += 1;
            }
            if at == self.batch_rows {
                self.batch_rows = 0;
                self.state = BatchState::Prefix { group, start };
                return Ok(());
            }
            sorted(BatchKind::Presorted, start..self.batch_start + at)?;
        }
    }
}

/// How a sort ordered its rows, under PostgreSQL's names.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum SortMethod {
    /// Kept only the rows a limit returns while reading the others.
    TopNHeapsort,
    /// Ordered every row in memory.
    Quicksort,
    /// Merged sorted runs written to the spill store.
    ExternalMerge,
}

impl SortMethod {
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::TopNHeapsort => "top-N heapsort",
            Self::Quicksort => "quicksort",
            Self::ExternalMerge => "external merge",
        }
    }

    /// PostgreSQL's name for where the sorted rows lived.
    pub(crate) const fn space_type(self) -> &'static str {
        match self {
            Self::TopNHeapsort | Self::Quicksort => "Memory",
            Self::ExternalMerge => "Disk",
        }
    }
}

/// What one sort did: its method and the bytes its rows took in memory or
/// in the spill store.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct SortSpace {
    pub(crate) method: SortMethod,
    pub(crate) bytes: usize,
}

/// A sort's row comparison.
pub(crate) type SortCompare<'c> =
    &'c mut dyn FnMut(&[u8], &[u8]) -> Result<core::cmp::Ordering, crate::sql::SqlError>;

/// The most columns a sort tuple sized as PostgreSQL's names.
const MAX_SORT_COLUMNS: usize = 64;

/// The tuples a PostgreSQL sort holds for rows pos3ql sorts as projected
/// rows: the values at its columns (every value when `all`), then `tids`
/// row identities; `bounded` names a sort a limit may bound, whose rows are
/// aset.c chunks.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SortTuples {
    columns: [u16; MAX_SORT_COLUMNS],
    count: usize,
    all: bool,
    tids: usize,
    bounded: bool,
}

impl SortTuples {
    /// `None` past the columns a sized tuple names.
    pub(crate) fn new(columns: Option<&[usize]>, tids: usize, bounded: bool) -> Option<Self> {
        let mut tuples = Self {
            columns: [0; MAX_SORT_COLUMNS],
            count: 0,
            all: columns.is_none(),
            tids,
            bounded,
        };
        for &column in columns.unwrap_or(&[]) {
            *tuples.columns.get_mut(tuples.count)? = u16::try_from(column).ok()?;
            tuples.count += 1;
        }
        Some(tuples)
    }

    fn length(&self, row: &[u8]) -> Option<usize> {
        let mut columns = [0usize; MAX_SORT_COLUMNS];
        for (column, &held) in columns.iter_mut().zip(&self.columns[..self.count]) {
            *column = usize::from(held);
        }
        super::postgres_memory::projected_tuple_len_with_tids(
            row,
            (!self.all).then_some(&columns[..self.count]),
            self.tids,
        )
    }

    fn width(&self, row: Option<&[u8]>) -> usize {
        let values = if self.all {
            row.map_or(0, crate::sql::exec::projected_row_width)
        } else {
            self.count
        };
        values + self.tids
    }

    /// How the sort copies its rows: a one-value tuple is a datum sort
    /// (`ExecInitSort`), a bounded sort keeps them in an aset.c context.
    /// `first` is the first row it read.
    fn copies(&self, first: Option<&[u8]>) -> super::postgres_memory::SortTuples {
        use super::postgres_memory::SortTuples as Copies;
        if self.width(first) == 1 {
            Copies::Datum
        } else if self.bounded {
            Copies::AllocSet
        } else {
            Copies::Bump
        }
    }

    /// `writetup_heap`'s bytes of a row whose minimal tuple is `length`
    /// bytes (its length word and its tuple after the header's padding), or
    /// `writetup_datum`'s of a datum sort's value.
    fn tape_bytes(
        &self,
        row: &[u8],
        length: usize,
        copies: super::postgres_memory::SortTuples,
    ) -> usize {
        match copies {
            // A datum sort of the row identity alone: a `tid` is six bytes.
            super::postgres_memory::SortTuples::Datum if !self.all && self.count == 0 => 4 + 6,
            super::postgres_memory::SortTuples::Datum => {
                4 + super::postgres_memory::sorted_datum_size(
                    row,
                    (!self.all).then(|| usize::from(self.columns[0])),
                )
            }
            _ => length - 10 + 4,
        }
    }

    /// A sort's accounting of rows read in arrival order, with the row
    /// count a limit bounds it to.
    pub(crate) fn model(
        &self,
        first: Option<&[u8]>,
        bound: Option<usize>,
    ) -> super::postgres_memory::TuplesortModel {
        super::postgres_memory::TuplesortModel::new(self.copies(first), bound)
    }

    /// Feeds one row to `model`; `false` when a value has no PostgreSQL
    /// size here.
    pub(crate) fn push(
        &self,
        model: &mut super::postgres_memory::TuplesortModel,
        copies_of_first: Option<&[u8]>,
        row: &[u8],
    ) -> bool {
        let Some(length) = self.length(row) else {
            return false;
        };
        let copies = self.copies(copies_of_first);
        model.push(length, self.tape_bytes(row, length, copies));
        true
    }

    /// What the rows of a bounded sort charge once its heap took them
    /// (`bounded_heap_charge`), over `count` rows in arrival order.
    pub(crate) fn heap_charge<'r>(
        &self,
        count: usize,
        heapify_at: usize,
        row_at: &dyn Fn(usize) -> &'r [u8],
        positions: &mut [usize],
        compare: SortCompare<'_>,
    ) -> Result<usize, crate::sql::SqlError> {
        use super::postgres_memory::{SortTuples as Copies, allocset_chunk_space, maxalign};
        let copies = self.copies((count > 0).then(|| row_at(0)));
        let charge = |length: usize| match copies {
            Copies::Datum => 0,
            Copies::Bump => maxalign(length),
            Copies::AllocSet => allocset_chunk_space(length),
        };
        let length = |row: usize| self.length(row_at(row)).unwrap_or(0);
        super::postgres_memory::bounded_heap_charge(
            count,
            heapify_at,
            |row| charge(length(row)),
            positions,
            |new, worst| Ok(compare(row_at(new), row_at(worst))?.is_lt()),
        )
    }

    /// What PostgreSQL's sort of `count` rows (`row_at`, in arrival order)
    /// reports (`tuplesort_get_stats` after the sort); with `heap`, a sort
    /// bounded to its first rows, whose bounded heap `compare` orders in a
    /// scratch array of the bound's positions. `None` when a value has no
    /// PostgreSQL size here.
    pub(crate) fn space<'r>(
        &self,
        count: usize,
        row_at: &dyn Fn(usize) -> &'r [u8],
        heap: Option<(&mut [usize], SortCompare<'_>)>,
        kept: Option<&mut bool>,
    ) -> Result<Option<super::postgres_memory::TuplesortSpace>, crate::sql::SqlError> {
        let first = (count > 0).then(|| row_at(0));
        let bound = heap.as_ref().map(|(positions, _)| positions.len());
        let mut model = self.model(first, bound.filter(|&bound| bound > 0));
        if kept.as_deref().copied().unwrap_or(false) {
            model = model.reuse_array();
        }
        for row in 0..count {
            if !self.push(&mut model, first, row_at(row)) {
                return Ok(None);
            }
        }
        let heap_charge = match heap {
            Some((positions, compare)) if model.bounded() => {
                Some(self.heap_charge(count, model.heap_at(), row_at, positions, compare)?)
            }
            _ => None,
        };
        if let Some(kept) = kept {
            *kept = !model.array_regrown();
        }
        Ok(Some(model.space(heap_charge)))
    }
}

impl SortSpace {
    /// PostgreSQL's sort of `rows` (in arrival order), whose tuples hold the
    /// values at `columns`; pos3ql's own in-memory figure when a value has
    /// no PostgreSQL size here.
    pub(crate) fn postgres(
        rows: &[&[u8]],
        columns: Option<&[usize]>,
        bounded: bool,
    ) -> Result<Self, crate::sql::SqlError> {
        Self::postgres_locking(rows, columns, 0, bounded, None)
    }

    /// [`Self::postgres`] of rows that also carry `tids` row identities a
    /// locking clause reads.
    pub(crate) fn postgres_locking(
        rows: &[&[u8]],
        columns: Option<&[usize]>,
        tids: usize,
        bounded: bool,
        kept: Option<&mut bool>,
    ) -> Result<Self, crate::sql::SqlError> {
        let own = Self::in_memory(rows, false);
        let space = match SortTuples::new(columns, tids, bounded) {
            Some(tuples) => tuples.space(rows.len(), &|row| rows[row], None, kept)?,
            None => None,
        };
        Self::from_postgres(space, own, false)
    }

    /// [`Self::postgres_locking`] of a sort a limit of `bound` rows bounds,
    /// over `rows` in arrival order: past twice the bound its heap keeps
    /// the rows `compare` puts first, as `make_bounded_heap` charges them.
    pub(crate) fn postgres_top_n(
        rows: &[&[u8]],
        columns: Option<&[usize]>,
        tids: usize,
        bound: usize,
        arena: &crate::mem::arena::Arena,
        mut compare: impl FnMut(&&[u8], &&[u8]) -> Result<core::cmp::Ordering, crate::sql::SqlError>,
        kept: Option<&mut bool>,
    ) -> Result<Self, crate::sql::SqlError> {
        let heap = rows.len() > bound * 2 && bound > 0;
        let own = Self::in_memory(rows, heap);
        let Some(tuples) = SortTuples::new(columns, tids, true) else {
            return Ok(own);
        };
        let positions = arena
            .alloc_slice_with(bound, |_| 0usize)
            .map_err(|_| super::arena_full_pub())?;
        let mut compare = |left: &[u8], right: &[u8]| compare(&left, &right);
        let space = tuples.space(
            rows.len(),
            &|row| rows[row],
            Some((positions, &mut compare)),
            kept,
        )?;
        Self::from_postgres(space, own, heap)
    }

    /// The figure PostgreSQL's sort reported, or `own` when a value has no
    /// PostgreSQL size here. `bounded` names a sort that kept only the
    /// rows its limit returns.
    pub(crate) fn from_postgres(
        space: Option<super::postgres_memory::TuplesortSpace>,
        own: Self,
        bounded: bool,
    ) -> Result<Self, crate::sql::SqlError> {
        use super::postgres_memory::TuplesortSpace;
        Ok(match space {
            None => own,
            Some(TuplesortSpace::Memory(bytes)) => Self {
                method: if bounded {
                    SortMethod::TopNHeapsort
                } else {
                    SortMethod::Quicksort
                },
                bytes,
            },
            Some(TuplesortSpace::Disk(bytes)) => Self {
                method: SortMethod::ExternalMerge,
                bytes,
            },
        })
    }

    /// An in-memory sort of `rows`, which pos3ql holds whole: their bytes
    /// and the array ordering them. `bounded` names a sort that kept only
    /// the rows its limit returns in its heap.
    pub(crate) fn in_memory(rows: &[&[u8]], bounded: bool) -> Self {
        let mut bytes = core::mem::size_of_val(rows);
        for row in rows {
            bytes = bytes.saturating_add(row.len());
        }
        Self {
            method: if bounded {
                SortMethod::TopNHeapsort
            } else {
                SortMethod::Quicksort
            },
            bytes,
        }
    }
}

/// The hash table a `Hash` node built.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(crate) struct HashTable {
    /// The rows it holds, which size PostgreSQL's buckets.
    pub(crate) tuples: u64,
    /// The bytes PostgreSQL's table holds for those rows: each row's
    /// `HashJoinTuple` header and minimal tuple. `None` when a hashed value
    /// has no PostgreSQL size here.
    pub(crate) tuple_bytes: Option<usize>,
    /// The bytes pos3ql's own arrays took, every array it outgrew included.
    pub(crate) own_bytes: usize,
    /// A table built as PostgreSQL's join builds it, batches included: its
    /// figures as `EXPLAIN ANALYZE` reports them.
    pub(crate) batches: Option<HashBatches>,
}

/// The figures of a hash table PostgreSQL's executor builds
/// (`HashInstrumentation`).
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(crate) struct HashBatches {
    pub(crate) buckets: u32,
    pub(crate) original_buckets: u32,
    pub(crate) batches: u32,
    pub(crate) original_batches: u32,
    /// `spacePeak`.
    pub(crate) peak: usize,
}

/// A value the rows of a `Hash` node hold, which sizes it as PostgreSQL
/// stores it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum HashColumn {
    /// The build relation's column at this position.
    Column(u16),
    /// The build relation's `tableoid`.
    TableOid,
    /// The row identity (`ctid`) a locking clause reads.
    RowIdentity,
    /// The build relation's whole row, a composite datum.
    WholeRow,
}

/// The most values a sized `Hash` row holds.
const MAX_HASH_COLUMNS: usize = 64;

/// The values of a `Hash` node's rows, in its target list's order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct HashColumns {
    columns: [HashColumn; MAX_HASH_COLUMNS],
    count: u8,
}

impl HashColumns {
    pub(crate) const EMPTY: Self = Self {
        columns: [HashColumn::TableOid; MAX_HASH_COLUMNS],
        count: 0,
    };

    /// Appends a value; `None` past the most a layout holds.
    pub(crate) fn push(&mut self, column: HashColumn) -> Option<()> {
        let slot = self.columns.get_mut(usize::from(self.count))?;
        *slot = column;
        self.count += 1;
        Some(())
    }

    pub(crate) fn as_slice(&self) -> &[HashColumn] {
        &self.columns[..usize::from(self.count)]
    }
}

/// `HJTUPLE_OVERHEAD`: a hash table entry's next pointer and hash value.
const HASH_TUPLE_OVERHEAD: usize = 16;
/// `SizeofMinimalTupleHeader`.
const MINIMAL_TUPLE_HEADER: usize = 15;
/// `VARATT_SHORT_MAX`: the largest varlena with a one-byte header.
const SHORT_VARLENA_MAX: usize = 127;

const fn align_to(offset: usize, alignment: u8) -> usize {
    let by = match alignment {
        b'd' => 8,
        b'i' => 4,
        b's' => 2,
        _ => 1,
    };
    offset.div_ceil(by) * by
}

use super::postgres_memory::varlena_payload;

/// The bytes PostgreSQL's hash table holds for one row: its `HashJoinTuple`
/// header and the minimal tuple (`heap_form_minimal_tuple`) of `columns`
/// read from the build row `values` of types `schema`. `None` when a value
/// has no PostgreSQL size here.
pub(crate) fn hashed_row_bytes(
    columns: &[HashColumn],
    values: &[crate::sql::types::Datum<'_>],
    schema: &[crate::sql::types::ColType],
) -> Option<usize> {
    let mut data = 0usize;
    let mut has_null = false;
    for &column in columns {
        let (length, alignment, storage, value) = match column {
            HashColumn::Column(index) => {
                let index = usize::from(index);
                let value = values.get(index)?;
                if value.is_null() {
                    has_null = true;
                    continue;
                }
                let layout = schema.get(index)?.physical_layout();
                (layout.length, layout.alignment, layout.storage, Some(value))
            }
            HashColumn::TableOid => (4, b'i', b'p', None),
            HashColumn::RowIdentity => (6, b's', b'p', None),
            HashColumn::WholeRow => {
                // A composite datum is a heap tuple whose header's first
                // word is its varlena length; it packs like any varlena.
                let payload = composite_bytes(values, schema)? - 4;
                if payload < SHORT_VARLENA_MAX {
                    data += payload + 1;
                } else {
                    data = align_to(data, b'd') + payload + 4;
                }
                continue;
            }
        };
        if length > 0 {
            data = align_to(data, alignment) + length as usize;
            continue;
        }
        if length != -1 {
            return None;
        }
        let payload = varlena_payload(value?)?;
        // A packable value short enough takes a one-byte header and no
        // alignment; others a four-byte header at the type's alignment.
        if storage != b'p' && payload < SHORT_VARLENA_MAX {
            data += payload + 1;
        } else {
            data = align_to(data, alignment) + payload + 4;
        }
    }
    let bitmap = if has_null {
        columns.len().div_ceil(8)
    } else {
        0
    };
    Some(HASH_TUPLE_OVERHEAD + align_to(MINIMAL_TUPLE_HEADER + bitmap, b'd') + data)
}

/// The bytes a `Materialize` node's tuplestore charges for one row holding
/// `columns` of `values` (of types `schema`): its minimal tuple, maxaligned,
/// in a memory chunk with its 8-byte header.
pub(crate) fn materialized_row_bytes(
    columns: &[HashColumn],
    values: &[crate::sql::types::Datum<'_>],
    schema: &[crate::sql::types::ColType],
) -> Option<usize> {
    hashed_row_bytes(columns, values, schema)
        .map(|bytes| align_to(bytes - HASH_TUPLE_OVERHEAD, b'd') + 8)
}

/// The most memory a tuplestore of `rows` rows of `row_bytes` in total
/// uses (`tuplestore_get_stats`): the rows and its array of row pointers,
/// 2048 at first and doubled when a row arrives at its last free slot, in
/// a chunk with its header.
pub(crate) fn tuplestore_bytes(rows: u64, row_bytes: usize) -> usize {
    let mut slots = 2048u64;
    while rows >= slots {
        slots *= 2;
    }
    row_bytes + slots as usize * core::mem::size_of::<u64>() + 8
}

/// The `t_len` of the minimal tuple of `columns` of `values` (of types
/// `schema`); `None` when a value has no PostgreSQL size here.
pub(crate) fn minimal_tuple_len(
    columns: &[HashColumn],
    values: &[crate::sql::types::Datum<'_>],
    schema: &[crate::sql::types::ColType],
) -> Option<usize> {
    hashed_row_bytes(columns, values, schema).map(|bytes| bytes - HASH_TUPLE_OVERHEAD)
}

/// The bytes of the composite datum of a row `values` of types `schema`
/// (`heap_form_tuple`): the tuple header with its null bitmap, aligned,
/// then each non-null value at its type's alignment.
fn composite_bytes(
    values: &[crate::sql::types::Datum<'_>],
    schema: &[crate::sql::types::ColType],
) -> Option<usize> {
    /// `offsetof(HeapTupleHeaderData, t_bits)`.
    const HEAP_TUPLE_HEADER: usize = 23;
    let values = values.get(..schema.len())?;
    let bitmap = if values.iter().any(|value| value.is_null()) {
        values.len().div_ceil(8)
    } else {
        0
    };
    let mut data = align_to(HEAP_TUPLE_HEADER + bitmap, b'd');
    for (value, column) in values.iter().zip(schema) {
        if value.is_null() {
            continue;
        }
        let layout = column.physical_layout();
        if layout.length > 0 {
            data = align_to(data, layout.alignment) + layout.length as usize;
            continue;
        }
        if layout.length != -1 {
            return None;
        }
        let payload = varlena_payload(value)?;
        if layout.storage != b'p' && payload < SHORT_VARLENA_MAX {
            data += payload + 1;
        } else {
            data = align_to(data, layout.alignment) + payload + 4;
        }
    }
    Some(data)
}

/// One plan node's counters while the statement runs.
#[derive(Clone, Copy)]
pub(crate) struct Slot {
    key: Option<NodeKey>,
    /// A `Hash` node's row values, which size its table as PostgreSQL's.
    hash_columns: Option<HashColumns>,
    /// A `Recursive Union`'s tuplestores, as its steps fill them.
    recursive: Option<RecursiveStores>,
    /// A hashed aggregate's table sizing, which sizes its memory as
    /// PostgreSQL's.
    hash_agg: Option<super::postgres_memory::HashAggSizing>,
    /// A `Materialize` node's tuplestore, as its first loop fills it; `None`
    /// inside once a row had no PostgreSQL size.
    store: Option<Option<super::postgres_memory::Tuplestore>>,
    /// How the node's page accesses are counted, once a scan installs it.
    pages: Option<PageModel>,
    page_loop: PageLoop,
    counters: NodeCounters,
    /// Nested starts of the open loop; only the outermost one counts.
    open: u32,
    started: Option<std::time::Instant>,
    first_row: Option<u64>,
    /// Rows a write's change loop finished qualifying or removing, which a
    /// later attempt does not see again: a restarted slot keeps them.
    carried: Option<(u64, u64)>,
    /// The attempt whose work the counters hold.
    attempt: u32,
    /// The run of a `WITH` query run again from its first row whose work
    /// the counters hold (see [`rerun`]); zero outside such runs.
    rerun: u32,
}

impl Slot {
    pub(crate) const fn new(key: Option<NodeKey>) -> Self {
        Self {
            key,
            hash_columns: None,
            recursive: None,
            hash_agg: None,
            store: None,
            pages: None,
            page_loop: PageLoop {
                last: None,
                heap: 0,
                read: 0,
                examined: 0,
                entries: 0,
                misses: 0,
            },
            counters: NodeCounters {
                loops: 0,
                rows: 0,
                filtered: 0,
                join_filtered: 0,
                startup_nanos: 0,
                total_nanos: 0,
                hash: None,
                sort: None,
                stored: None,
                stored_disk: false,
                hash_agg_peak: None,
                cte_stored: None,
                incremental: None,
                merged: MergeTuples {
                    inserted: 0,
                    updated: 0,
                    deleted: 0,
                },
                heap_blocks: 0,
                heap_fetches: 0,
                memoize: None,
                buffers: BufferUsage {
                    shared_hit: 0,
                    shared_read: 0,
                    shared_dirtied: 0,
                    shared_written: 0,
                    local_hit: 0,
                    local_read: 0,
                    local_dirtied: 0,
                    local_written: 0,
                    temp_read: 0,
                    temp_written: 0,
                },
                paged: false,
            },
            open: 0,
            started: None,
            first_row: None,
            carried: None,
            attempt: 0,
            rerun: 0,
        }
    }

    /// Restarts the counters for `attempt`, which reached the node after an
    /// earlier attempt counted it, keeping the rows the earlier attempts'
    /// write finished.
    /// The slot of a `Hash` node whose rows hold `columns`.
    pub(crate) const fn with_hash_columns(mut self, columns: Option<HashColumns>) -> Self {
        self.hash_columns = columns;
        self
    }

    /// The slot of a hashed aggregate whose table `sizing` sizes.
    pub(crate) const fn with_hash_agg(
        mut self,
        sizing: Option<super::postgres_memory::HashAggSizing>,
    ) -> Self {
        self.hash_agg = sizing;
        self
    }

    fn restart(&mut self, attempt: u32) {
        let (carried, rerun) = (self.carried, self.rerun);
        *self = Self::new(self.key)
            .with_hash_columns(self.hash_columns)
            .with_hash_agg(self.hash_agg);
        self.attempt = attempt;
        self.rerun = rerun;
        if let Some((rows, filtered)) = carried {
            self.carried = carried;
            self.counters.rows = rows;
            self.counters.filtered = filtered;
        }
    }

    pub(crate) const fn counters(&self) -> NodeCounters {
        self.counters
    }

    /// Adds the ending loop's page accesses: the heap pages it read, the
    /// page a sequential scan's read stream holds past the one it stopped
    /// on, its index search's pages and a visibility map's page.
    fn end_page_loop(&mut self) {
        let Some(model) = self.pages else {
            return;
        };
        let pages = self.page_loop;
        let ahead = model.sequential
            && pages.examined < model.rows
            && pages
                .last
                .is_some_and(|last| last + 1 < model.relation_pages);
        let index = model.index.map_or(0, |index| index.search(pages.entries));
        let map = model.visibility_map && self.counters.loops == 0;
        let accessed = pages.heap + u64::from(ahead) + index + u64::from(map);
        self.counters.buffers =
            self.counters
                .buffers
                .plus(BufferUsage::accessed(model.local, accessed, pages.read));
        self.page_loop = PageLoop {
            misses: pages.misses,
            ..PageLoop::default()
        };
    }
}

struct Registry {
    slots: NonNull<[Slot]>,
    timing: bool,
    /// `BUFFERS`: scans count the pages they access.
    buffers: bool,
    /// Tables' heap page maps, built once per statement for the scans
    /// that count pages (see [`heap_pages`]).
    heap_pages: [Option<HeapPages>; HEAP_PAGE_TABLES],
    /// For a statement rewrite rules turn into several queries, the
    /// position of the next query to run.
    next_query: Option<u16>,
    /// The statement's current attempt.
    attempt: u32,
    /// The runs [`rerun`] started.
    reruns: u32,
}

thread_local! {
    static ACTIVE: Cell<Option<NonNull<Registry>>> = const { Cell::new(None) };
    static BLOCK: Cell<BlockId> = const { Cell::new(BlockId::ROOT) };
    /// The [`rerun`] whose nodes are counting, or zero.
    static RERUN: Cell<u32> = const { Cell::new(0) };
}

/// Counts a `WITH` query run again from its first row, to reach rows past
/// where an earlier run stopped, until the returned scope ends: each node
/// it reaches restarts its counters there, so the nodes report the rows up
/// to the furthest row read, once, as PostgreSQL's single run of the query
/// does.
#[must_use]
pub(crate) fn rerun() -> Rerun {
    let Some(mut registry) = ACTIVE.with(Cell::get) else {
        return Rerun(RERUN.with(Cell::get));
    };
    // SAFETY: see `node`; nothing else borrows the registry here.
    let registry = unsafe { registry.as_mut() };
    registry.reruns = registry.reruns.wrapping_add(1).max(1);
    Rerun(RERUN.with(|rerun| rerun.replace(registry.reruns)))
}

/// Counts the reader of a rerun query's rows normally until the returned
/// scope ends.
#[must_use]
pub(crate) fn outside_rerun() -> Rerun {
    Rerun(RERUN.with(|rerun| rerun.replace(0)))
}

pub(crate) struct Rerun(u32);

impl Drop for Rerun {
    fn drop(&mut self) {
        RERUN.with(|rerun| rerun.set(self.0));
    }
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
/// block. `timing` measures each loop's time as well, `buffers` the pages
/// each scan accesses.
pub(crate) fn analyze<R>(
    slots: &mut [Slot],
    timing: bool,
    buffers: bool,
    execute: impl FnOnce() -> R,
) -> R {
    install(slots, timing, buffers, None, execute)
}

/// [`analyze`] of a statement rewrite rules turn into several queries, each
/// counted in its own block as [`next_query`] starts it.
pub(crate) fn analyze_queries<R>(
    slots: &mut [Slot],
    timing: bool,
    buffers: bool,
    execute: impl FnOnce() -> R,
) -> R {
    install(slots, timing, buffers, Some(0), execute)
}

fn install<R>(
    slots: &mut [Slot],
    timing: bool,
    buffers: bool,
    next_query: Option<u16>,
    execute: impl FnOnce() -> R,
) -> R {
    let mut registry = Registry {
        slots: NonNull::from(slots),
        timing,
        buffers,
        heap_pages: [None; HEAP_PAGE_TABLES],
        next_query,
        attempt: 0,
        reruns: 0,
    };
    let _restore = Restore {
        active: ACTIVE.with(|active| active.replace(Some(NonNull::from(&mut registry)))),
        block: BLOCK.with(|block| block.replace(BlockId::ROOT)),
    };
    execute()
}

/// Starts the next of the queries rewrite rules turned the analyzed
/// statement into: its operators count in that query's block. The executor
/// runs the queries in the order EXPLAIN lists them.
pub(crate) fn next_query() {
    let Some(mut registry) = ACTIVE.with(Cell::get) else {
        return;
    };
    // SAFETY: see `node`; nothing else borrows the registry here.
    let registry = unsafe { registry.as_mut() };
    if let Some(position) = registry.next_query {
        BLOCK.with(|block| block.set(BlockId::ROOT.child(Segment::Query(position))));
        registry.next_query = Some(position.saturating_add(1));
    }
}

/// Starts another attempt of the analyzed statement, which runs it again
/// from its first row after an earlier attempt stopped for a routine call:
/// each node's counters restart when this attempt first reaches it, so they
/// report the last attempt that ran the node.
pub(crate) fn restart() {
    let Some(mut registry) = ACTIVE.with(Cell::get) else {
        return;
    };
    // SAFETY: see `node`; no `Node` of the statement is open between
    // attempts, and nothing else borrows the registry here.
    let registry = unsafe { registry.as_mut() };
    registry.attempt = registry.attempt.wrapping_add(1);
    // A later attempt may run in a fresh workspace.
    registry.heap_pages = [None; HEAP_PAGE_TABLES];
    if registry.next_query.is_some() {
        registry.next_query = Some(0);
    }
    BLOCK.with(|block| block.set(BlockId::ROOT));
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

/// A `Recursive Union`'s working and intermediate tables, and the store of
/// every row its `CTE Scan`s read.
#[derive(Clone, Copy)]
struct RecursiveStores {
    tables: [super::postgres_memory::Tuplestore; 2],
    scanned: super::postgres_memory::Tuplestore,
    steps: usize,
    /// The current step added a row.
    filling: bool,
    unsized_rows: bool,
}

impl RecursiveStores {
    fn new() -> Self {
        let empty = super::postgres_memory::Tuplestore::new();
        Self {
            tables: [empty; 2],
            scanned: empty,
            steps: 0,
            filling: false,
            unsized_rows: false,
        }
    }
}

/// What a tuplestore held at most, as `tuplestore_get_stats` reports it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum StoredSpace {
    Sized {
        disk: bool,
        bytes: usize,
    },
    /// The rows held a value whose PostgreSQL size pos3ql does not model.
    Unsized,
}

/// Records what the tuplestore of the `WITH` query run in `block` held, on
/// that block's first node.
pub(crate) fn cte_stored(block: BlockId, space: StoredSpace) {
    let Some(registry) = ACTIVE.with(Cell::get) else {
        return;
    };
    // SAFETY: see `node`; no reference into a slot is held across calls.
    let registry = unsafe { registry.as_ref() };
    let slots = unsafe { &mut *registry.slots.as_ptr() };
    // A query of one `SELECT` runs as the leaf of its set tree.
    let leaf = block.child(Segment::Branch(0));
    let slot = slots.iter_mut().find(|slot| {
        slot.key
            .is_some_and(|key| key.block == block || key.block == leaf)
    });
    // Not a loop of the node: an attempt that reads the rows an earlier one
    // ran leaves the counters that attempt kept.
    if let Some(slot) = slot {
        slot.counters.cte_stored = Some(space);
    }
}

/// Whether an analyzed statement is counting on this thread.
pub(crate) fn active() -> bool {
    ACTIVE.with(Cell::get).is_some()
}

/// The tables whose heap page maps one analyzed statement keeps.
const HEAP_PAGE_TABLES: usize = 16;

/// A table's `(row identity, heap page)` pairs sorted by identity, and its
/// heap pages.
pub(crate) type HeapPageMap<'a> = (&'a [(u64, u64)], u64);

/// A [`HeapPageMap`] the analyzed statement keeps for the table in `slot`.
#[derive(Clone, Copy)]
struct HeapPages {
    slot: usize,
    rows: NonNull<[(u64, u64)]>,
    pages: u64,
}

/// The heap page map of the table in `slot`, built by `build` in the
/// persistent tail of the statement's owning `arena` the first time a scan
/// of the analyzed statement asks for it, so a scan run once per outer row
/// does not build it again.
pub(crate) fn heap_pages<'a, E>(
    slot: usize,
    arena: &'a crate::mem::arena::Arena,
    build: impl FnOnce(&'a crate::mem::arena::Arena) -> Result<HeapPageMap<'a>, E>,
) -> Result<HeapPageMap<'a>, E> {
    let Some(mut registry) = ACTIVE.with(Cell::get) else {
        return build(arena);
    };
    // SAFETY: see `node`; nothing else borrows the registry here.
    let registry = unsafe { registry.as_mut() };
    if let Some(held) = registry
        .heap_pages
        .iter()
        .flatten()
        .find(|held| held.slot == slot)
    {
        // SAFETY: the map lives in the owning arena's persistent tail,
        // which no front rewind reclaims and which outlives the statement's
        // analysis; a restarted attempt forgets it.
        return Ok((unsafe { held.rows.as_ref() }, held.pages));
    }
    let owner = arena.retention_owner();
    let (rows, pages) = build(owner)?;
    if let Some(free) = registry.heap_pages.iter_mut().find(|held| held.is_none()) {
        *free = Some(HeapPages {
            slot,
            rows: NonNull::from(rows),
            pages,
        });
    }
    Ok((rows, pages))
}

/// Whether the analyzed statement counts the pages its scans access.
pub(crate) fn buffers_counted() -> bool {
    // SAFETY: see `node`; the registry outlives its installation.
    ACTIVE
        .with(Cell::get)
        .is_some_and(|registry| unsafe { registry.as_ref() }.buffers)
}

/// The block the executor is running.
pub(crate) fn current_block() -> BlockId {
    BLOCK.with(Cell::get)
}

/// Runs nested blocks in `block` until the returned scope ends: a query
/// planned in one block that runs later, from wherever it is first read.
#[must_use]
pub(crate) fn enter_block(block: BlockId) -> Entered {
    Entered(BLOCK.with(|current| current.replace(block)))
}

thread_local! {
    /// The query of the subquery the executor is running, which a nested
    /// runner of the same subquery does not enter again.
    static SUBLINK: Cell<u64> = const { Cell::new(0) };
}

/// Runs the subquery whose query is `query` in its own block, nested in the
/// current one, until the returned scope ends. A subquery's runners nest
/// (a row subquery runs its set): only the outermost enters.
#[must_use]
pub(crate) fn enter_sublink(query: &crate::sql::ast::Select<'_>) -> SublinkEntered {
    let query = query.instrument_identity();
    if SUBLINK.with(Cell::get) == query {
        return SublinkEntered(None);
    }
    let prior = SUBLINK.with(|current| current.replace(query));
    SublinkEntered(Some((enter(Segment::Sublink(query)), prior)))
}

pub(crate) struct SublinkEntered(Option<(Entered, u64)>);

impl Drop for SublinkEntered {
    fn drop(&mut self) {
        if let Some((_, prior)) = &self.0 {
            SUBLINK.with(|current| current.set(*prior));
        }
    }
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
        attempt: registry.attempt,
    }
}

/// An operator's counters, or nothing outside `EXPLAIN ANALYZE`.
#[derive(Clone, Copy)]
pub(crate) struct Node {
    slot: Option<NonNull<Slot>>,
    timing: bool,
    /// The attempt the node counts for.
    attempt: u32,
}

impl Node {
    pub(crate) const NONE: Self = Self {
        slot: None,
        timing: false,
        attempt: 0,
    };

    fn with(self, update: impl FnOnce(&mut Slot, bool)) {
        if let Some(mut slot) = self.slot {
            // SAFETY: see `node`; the executor is single-threaded and no
            // reference into a slot is held across calls.
            let slot = unsafe { slot.as_mut() };
            if slot.attempt != self.attempt {
                slot.restart(self.attempt);
            }
            let rerun = RERUN.with(Cell::get);
            if rerun != 0 && slot.rerun != rerun {
                slot.restart(slot.attempt);
                slot.rerun = rerun;
            }
            update(slot, self.timing);
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

    /// The rows the node returned and its `Filter` removed so far.
    pub(crate) fn counted(self) -> (u64, u64) {
        let mut counted = (0, 0);
        self.with(|slot, _| counted = (slot.counters.rows, slot.counters.filtered));
        counted
    }

    /// Adds rows another node returned and removed while this one ran in
    /// it: a member scan's share of its `Append`'s counts.
    pub(crate) fn add_counted(self, rows: u64, filtered: u64) {
        self.rows(rows);
        self.with(|slot, _| {
            slot.counters.filtered = slot.counters.filtered.saturating_add(filtered)
        });
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

    /// The values of the rows this `Hash` node holds, when the plan sized
    /// them.
    pub(crate) fn hash_columns(self) -> Option<HashColumns> {
        let mut columns = None;
        self.with(|slot, _| columns = slot.hash_columns);
        columns
    }

    /// Records the hash table the node built; across the builds of rescans
    /// PostgreSQL reports the largest table.
    pub(crate) fn hash_table(self, table: HashTable) {
        self.with(|slot, _| {
            slot.counters.hash = Some(match slot.counters.hash {
                None => table,
                Some(held) => HashTable {
                    tuples: held.tuples.max(table.tuples),
                    tuple_bytes: held
                        .tuple_bytes
                        .zip(table.tuple_bytes)
                        .map(|(held, built)| held.max(built)),
                    own_bytes: held.own_bytes.max(table.own_bytes),
                    // `ExecHashAccumInstrumentation`: the largest of each.
                    batches: match (held.batches, table.batches) {
                        (Some(held), Some(built)) => Some(HashBatches {
                            buckets: held.buckets.max(built.buckets),
                            original_buckets: held.original_buckets.max(built.original_buckets),
                            batches: held.batches.max(built.batches),
                            original_batches: held.original_batches.max(built.original_batches),
                            peak: held.peak.max(built.peak),
                        }),
                        (held, built) => built.or(held),
                    },
                },
            });
        });
    }

    /// The sizing of this hashed aggregate's table, when the plan has one.
    pub(crate) fn hash_agg_sizing(self) -> Option<super::postgres_memory::HashAggSizing> {
        let mut sizing = None;
        self.with(|slot, _| sizing = slot.hash_agg);
        sizing
    }

    /// Records a `Memoize` node's cache figures.
    pub(crate) fn memoize(self, stats: MemoizeStats) {
        self.with(|slot, _| slot.counters.memoize = Some(stats));
    }

    /// Records a hashed aggregate's peak memory; across rescans PostgreSQL
    /// reports the most.
    pub(crate) fn hash_agg_peak(self, bytes: usize) {
        self.with(|slot, _| {
            slot.counters.hash_agg_peak = Some(
                slot.counters
                    .hash_agg_peak
                    .map_or(bytes, |held| held.max(bytes)),
            );
        });
    }

    /// Counts a row a `MERGE` action changed.
    pub(crate) fn merged(self, change: MergeChange) {
        self.with(|slot, _| {
            let merged = &mut slot.counters.merged;
            let count = match change {
                MergeChange::Insert => &mut merged.inserted,
                MergeChange::Update => &mut merged.updated,
                MergeChange::Delete => &mut merged.deleted,
            };
            *count = count.saturating_add(1);
        });
    }

    /// Records the batches an incremental sort sorted.
    pub(crate) fn incremental_sorted(self, groups: IncrementalGroups) {
        self.with(|slot, _| slot.counters.incremental = Some(groups));
    }

    /// Records a sort the node ran; across rescans PostgreSQL reports the
    /// one that used the most space, preferring one that spilled.
    pub(crate) fn sorted(self, space: SortSpace) {
        self.with(|slot, _| {
            let rank = |space: SortSpace| (space.method == SortMethod::ExternalMerge, space.bytes);
            if slot
                .counters
                .sort
                .is_none_or(|held| rank(space) >= rank(held))
            {
                slot.counters.sort = Some(space);
            }
            // An external merge writes its runs to work files and reads
            // them back, in pages.
            if space.method == SortMethod::ExternalMerge {
                let blocks = space.bytes.div_ceil(8192) as u64;
                slot.counters.buffers = slot.counters.buffers.plus(BufferUsage {
                    temp_read: blocks,
                    temp_written: blocks,
                    ..BufferUsage::default()
                });
            }
        });
    }

    /// Counts the rows a `Recursive Union`'s step added (its last step
    /// added none): the tuplestores `nodeRecursiveunion.c` swaps between its
    /// working and intermediate tables, each cleared before it is refilled,
    /// and the store its `CTE Scan`s read, which holds every row.
    pub(crate) fn recursive_step(self, rows: &[&[u8]]) {
        for row in rows {
            self.recursive_row(row);
        }
        self.recursive_step_end();
    }

    /// Adds one row of the current `Recursive Union` step (see
    /// [`Self::recursive_step`]).
    pub(crate) fn recursive_row(self, row: &[u8]) {
        self.with(|slot, _| {
            let stores = slot.recursive.get_or_insert_with(RecursiveStores::new);
            if stores.unsized_rows {
                return;
            }
            if !stores.filling {
                stores.filling = true;
                if stores.steps >= 2 {
                    stores.tables[stores.steps % 2].clear();
                }
            }
            let Some(length) = super::postgres_memory::projected_tuple_len(row, None) else {
                stores.unsized_rows = true;
                slot.counters.cte_stored = Some(StoredSpace::Unsized);
                return;
            };
            stores.tables[stores.steps % 2].put(length);
            stores.scanned.put(length);
        });
    }

    /// Ends a `Recursive Union` step.
    pub(crate) fn recursive_step_end(self) {
        self.with(|slot, _| {
            let stores = slot.recursive.get_or_insert_with(RecursiveStores::new);
            if stores.unsized_rows {
                return;
            }
            if !stores.filling && stores.steps >= 2 {
                stores.tables[stores.steps % 2].clear();
            }
            stores.filling = false;
            stores.steps += 1;
            let (_, working) = stores.tables[0].finish();
            let (_, intermediate) = stores.tables[1].finish();
            slot.counters.stored = Some(working + intermediate);
            let (disk, bytes) = stores.scanned.finish();
            slot.counters.cte_stored = Some(StoredSpace::Sized { disk, bytes });
        });
    }

    /// Opens the node's tuplestore, which reports its memory even when no
    /// row enters it.
    pub(crate) fn store_opened(self) {
        self.with(|slot, _| {
            if slot.store.is_none() {
                let store = super::postgres_memory::Tuplestore::new();
                slot.counters.stored = Some(store.finish().1);
                slot.store = Some(Some(store));
            }
        });
    }

    /// Stores a row whose minimal tuple is `length` bytes (`None` without a
    /// PostgreSQL size) in the node's tuplestore.
    pub(crate) fn store_row(self, length: Option<usize>) {
        self.with(|slot, _| {
            let store = slot
                .store
                .get_or_insert_with(|| Some(super::postgres_memory::Tuplestore::new()));
            match (store.as_mut(), length) {
                (Some(store), Some(length)) => {
                    store.put(length);
                    slot.counters.stored = Some(store.finish().1);
                }
                _ => {
                    *store = None;
                    slot.counters.stored = None;
                }
            }
        });
    }

    /// Records the bytes of rows the node held, keeping the most.
    pub(crate) fn stored(self, bytes: usize) {
        self.with(|slot, _| {
            slot.counters.stored = Some(slot.counters.stored.map_or(bytes, |held| held.max(bytes)));
        });
    }

    /// [`Self::stored`] for a buffer that may have gone to disk.
    pub(crate) fn stored_space(self, disk: bool, bytes: usize) {
        self.stored(bytes);
        self.with(|slot, _| slot.counters.stored_disk |= disk);
    }

    /// Marks the start of a candidate row in a write's change loop: the
    /// rows qualified or removed before it are final. An attempt that
    /// resumes the write starts at the row it left unfinished, so
    /// [`restart`] keeps the counts up to that row's start.
    pub(crate) fn checkpoint(self) {
        self.with(|slot, _| {
            slot.carried = Some((slot.counters.rows, slot.counters.filtered));
        });
    }

    /// Counts a heap page a bitmap heap scan read.
    pub(crate) fn heap_block(self) {
        self.with(|slot, _| {
            slot.counters.heap_blocks = slot.counters.heap_blocks.saturating_add(1)
        });
    }

    /// Counts the node's page accesses by `model` from now on.
    pub(crate) fn page_model(self, model: PageModel) {
        self.with(|slot, _| {
            if slot.pages.is_none() {
                slot.page_loop.misses = crate::store::thread_misses();
            }
            slot.pages = Some(model);
            slot.counters.paged = true;
        });
    }

    /// Counts a read of heap page `page`, unless it is the page the scan
    /// read last.
    pub(crate) fn heap_page(self, page: u64) {
        self.with(|slot, _| {
            let pages = &mut slot.page_loop;
            if slot.pages.is_none() || pages.last == Some(page) {
                return;
            }
            pages.last = Some(page);
            pages.heap += 1;
            let misses = crate::store::thread_misses();
            if misses != pages.misses {
                pages.misses = misses;
                pages.read += 1;
            }
        });
    }

    /// Counts a row a sequential scan examined.
    pub(crate) fn examined(self) {
        self.with(|slot, _| {
            if slot.pages.is_some() {
                slot.page_loop.examined += 1;
            }
        });
    }

    /// Counts an index entry the node's search reached.
    pub(crate) fn index_entry(self) {
        self.with(|slot, _| {
            if slot.pages.is_some() {
                slot.page_loop.entries += 1;
            }
        });
    }

    /// Counts a row an index-only scan read from its heap page.
    pub(crate) fn heap_fetch(self) {
        self.with(|slot, _| {
            slot.counters.heap_fetches = slot.counters.heap_fetches.saturating_add(1);
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

    /// Counts `count` rows the node's `Join Filter` removed.
    pub(crate) fn add_join_filtered(self, count: u64) {
        self.with(|slot, _| {
            slot.counters.join_filtered = slot.counters.join_filtered.saturating_add(count);
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

    /// Opens one step of the node's single execution: a node run in
    /// several steps (a data-modifying `WITH` query its readers run a row at
    /// a time) adds each step's time to one loop.
    pub(crate) fn step(self) -> Step {
        self.begin();
        Step(self)
    }

    fn end_step(self) {
        self.with(|slot, _| {
            slot.open = slot.open.saturating_sub(1);
            if slot.open != 0 {
                return;
            }
            slot.end_page_loop();
            let first = slot.counters.loops == 0;
            slot.counters.loops = 1;
            if let Some(started) = slot.started.take() {
                let total = elapsed_nanos(started);
                slot.counters.total_nanos = slot.counters.total_nanos.saturating_add(total);
                if first {
                    slot.counters.startup_nanos = slot.first_row.take().unwrap_or(total);
                }
            }
        });
    }

    fn end(self) {
        self.with(|slot, _| {
            // A loop a rerun's restart closed does not count again.
            if slot.open == 0 {
                return;
            }
            slot.open -= 1;
            if slot.open != 0 {
                return;
            }
            slot.end_page_loop();
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

/// One open step of a node's single execution.
#[must_use]
pub(crate) struct Step(Node);

impl Drop for Step {
    fn drop(&mut self) {
        self.0.end_step();
    }
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
/// PostgreSQL's order from the top: limit, row locks, distinct, sort,
/// set-returning projection, window, aggregation.
#[derive(Clone, Copy)]
pub(crate) struct Stages {
    pub(crate) limit: Node,
    pub(crate) lock_rows: Node,
    pub(crate) unique: Node,
    pub(crate) sort: Node,
    pub(crate) project_set: Node,
    pub(crate) window: Node,
    pub(crate) aggregate: Node,
}

impl Stages {
    /// The stages of the current block that the explained plan has.
    pub(crate) fn current() -> Self {
        if !active() {
            return Self {
                limit: Node::NONE,
                lock_rows: Node::NONE,
                unique: Node::NONE,
                sort: Node::NONE,
                project_set: Node::NONE,
                window: Node::NONE,
                aggregate: Node::NONE,
            };
        }
        Self {
            limit: node(Role::Limit),
            lock_rows: node(Role::LockRows),
            unique: node(Role::Unique),
            sort: node(Role::Sort),
            project_set: node(Role::ProjectSet),
            window: node(Role::Window),
            aggregate: node(Role::Aggregate),
        }
    }

    /// The stage the limit reads its rows from.
    pub(crate) fn below_limit(&self) -> Node {
        [
            self.lock_rows,
            self.unique,
            self.sort,
            self.project_set,
            self.window,
            self.aggregate,
        ]
        .into_iter()
        .find(|node| node.measured())
        .unwrap_or(Node::NONE)
    }

    /// Opens one execution of every stage.
    pub(crate) fn start(&self) -> StageLoops {
        let nodes = [
            self.limit,
            self.lock_rows,
            self.unique,
            self.sort,
            self.project_set,
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
pub(crate) struct StageLoops([Node; 7]);

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

#[cfg(all(test, not(feature = "sql-fixture-suite")))]
mod tests {
    use super::*;

    #[test]
    fn hashed_rows_take_postgresql_minimal_tuple_sizes() {
        use crate::sql::numeric::{Numeric, Sign};
        use crate::sql::types::{ColType, Datum};
        let schema = [
            ColType::Int4,
            ColType::Text,
            ColType::Numeric,
            ColType::Int8,
        ];
        let mut columns = HashColumns::EMPTY;
        for column in [0, 1, 2, 3] {
            columns.push(HashColumn::Column(column)).unwrap();
        }
        let digits = [0x34, 0x12, 0x01, 0x00];
        let numeric = Numeric {
            sign: Sign::Pos,
            weight: 1,
            dscale: 2,
            digits: &digits,
        };
        let long = "x".repeat(130);
        crate::mem::guard::forbid_alloc(|| {
            // An int, a short text with a one-byte header, a short numeric
            // (2-byte header, two digits) and an int8 aligned to 8.
            let values = [
                Datum::Int4(7),
                Datum::Text("ab"),
                Datum::Numeric(numeric),
                Datum::Int8(1),
            ];
            assert_eq!(
                hashed_row_bytes(columns.as_slice(), &values, &schema),
                // 4 + 3 + 7 bytes, padded to 16, then 8.
                Some(16 + 16 + 24)
            );
            // A NULL adds the null bitmap; a long text takes a four-byte
            // header at four-byte alignment.
            let values = [
                Datum::Null,
                Datum::Text(&long),
                Datum::Numeric(Numeric::NAN),
                Datum::Int8(1),
            ];
            assert_eq!(
                hashed_row_bytes(columns.as_slice(), &values, &schema),
                // 134 + 3 bytes, padded to 144, then 8.
                Some(16 + 16 + 152)
            );
            let mut identity = HashColumns::EMPTY;
            identity.push(HashColumn::TableOid).unwrap();
            identity.push(HashColumn::RowIdentity).unwrap();
            assert_eq!(
                hashed_row_bytes(identity.as_slice(), &[], &[]),
                Some(16 + 16 + 10)
            );
            let array = [Datum::Array {
                element: crate::sql::types::ArrElem::Int4,
                raw: &[],
            }];
            let mut first = HashColumns::EMPTY;
            first.push(HashColumn::Column(0)).unwrap();
            assert_eq!(
                hashed_row_bytes(
                    first.as_slice(),
                    &array,
                    &[ColType::Array(crate::sql::types::ArrElem::Int4)]
                ),
                None
            );
        });
    }

    #[test]
    fn counts_reach_only_the_named_node_of_the_current_block() {
        let scan = NodeKey::new(BlockId::ROOT, Role::Scan(0));
        let nested = NodeKey::new(BlockId::ROOT.child(Segment::Derived(1)), Role::Scan(0));
        let mut slots = [
            Slot::new(Some(scan)),
            Slot::new(Some(nested)),
            Slot::new(None),
        ];
        analyze(&mut slots, false, false, || {
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
    fn each_node_reports_the_last_attempt_that_ran_it() {
        let write = NodeKey::new(BlockId::ROOT.child(Segment::Cte(0)), Role::Scan(0));
        let read = NodeKey::new(BlockId::ROOT, Role::Scan(0));
        let mut slots = [Slot::new(Some(write)), Slot::new(Some(read))];
        analyze(&mut slots, false, false, || {
            restart();
            {
                let _cte = enter(Segment::Cte(0));
                let scan = node(Role::Scan(0));
                let _loop = scan.start();
                scan.rows(4);
                scan.filtered();
            }
            let scan = node(Role::Scan(0));
            {
                let _loop = scan.start();
                scan.rows(2);
            }
            // The next attempt keeps the write and reads again.
            restart();
            let scan = node(Role::Scan(0));
            let _loop = scan.start();
            scan.rows(3);
        });
        let kept = slots[0].counters();
        assert_eq!((kept.loops, kept.rows, kept.filtered), (1, 4, 1));
        let read = slots[1].counters();
        assert_eq!((read.loops, read.rows), (1, 3));
    }

    #[test]
    fn a_node_run_in_steps_counts_one_loop() {
        let modify = NodeKey::new(BlockId::ROOT, Role::Modify);
        let mut slots = [Slot::new(Some(modify))];
        analyze(&mut slots, false, false, || {
            for _ in 0..3 {
                let node = node(Role::Modify);
                let _step = node.step();
                node.row();
            }
        });
        let counters = slots[0].counters();
        assert_eq!((counters.loops, counters.rows), (1, 3));
    }

    fn batches(groups: &[usize]) -> (u64, u64) {
        let mut batches = IncrementalSortBatches::new();
        let mut counts = (0u64, 0u64);
        let mut count = |kind: BatchKind, _: core::ops::Range<usize>| {
            match kind {
                BatchKind::Full => counts.0 += 1,
                BatchKind::Presorted => counts.1 += 1,
            }
            Ok(())
        };
        let mut at = 0usize;
        for &size in groups {
            for row in 0..size {
                batches.push(row == 0, at, &mut count).unwrap();
                at += 1;
            }
        }
        batches.finish(at, &mut count).unwrap();
        counts
    }

    #[test]
    fn incremental_sort_batches_follow_postgresql() {
        assert_eq!(batches(&[50, 50, 50, 50]), (4, 0));
        assert_eq!(batches(&[10, 30, 260, 10]), (3, 1));
        // A full sort that passes 64 rows sorts its leading groups apart.
        assert_eq!(batches(&[40, 30, 100]), (2, 2));
        assert_eq!(batches(&[]), (1, 0));
    }

    #[test]
    fn suspended_bodies_count_nothing() {
        let scan = NodeKey::new(BlockId::ROOT, Role::Scan(0));
        let mut slots = [Slot::new(Some(scan))];
        analyze(&mut slots, true, false, || {
            let _body = suspend();
            let scan = node(Role::Scan(0));
            let _loop = scan.start();
            scan.row();
        });
        assert_eq!(slots[0].counters().loops, 0);
    }
}
