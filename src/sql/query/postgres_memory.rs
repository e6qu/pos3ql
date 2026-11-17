//! PostgreSQL 18.6's memory accounting behind the figures `EXPLAIN ANALYZE`
//! prints: a sort's `Memory`, a tuplestore's `Maximum Storage` and a hash
//! aggregate's `Memory Usage`. Each is arithmetic over the chunk and block
//! sizes PostgreSQL's allocators charge, which pos3ql reproduces from the
//! sizes of the rows the node held.
//!
//! Sources (REL_18_6): `utils/sort/tuplesort.c`, `utils/sort/tuplestore.c`,
//! `executor/nodeAgg.c`, `executor/execGrouping.c`, `lib/simplehash.h`,
//! `utils/mmgr/aset.c`, `utils/mmgr/bump.c` and `utils/mmgr/generation.c`.

use crate::sql::type_layouts::PhysicalLayout;
use crate::sql::types::Datum;

/// `MAXALIGN` on the supported 64-bit targets.
pub(crate) const fn maxalign(bytes: usize) -> usize {
    bytes.div_ceil(8) * 8
}

/// `sizeof(MemoryChunk)`: every aset.c and generation.c chunk's header.
const CHUNK_HEADER: usize = 8;
/// aset.c's `ALLOC_CHUNK_LIMIT`: larger requests get a block of their own.
const ALLOC_CHUNK_LIMIT: usize = 8192;
/// aset.c's `ALLOC_BLOCKHDRSZ`.
const ALLOC_BLOCK_HEADER: usize = 40;
/// `MAXALIGN(sizeof(AllocSetContext))`.
const ALLOC_SET_CONTEXT: usize = 200;
/// `ALLOCSET_DEFAULT_INITSIZE`.
const DEFAULT_INIT_BLOCK: usize = 8 * 1024;
/// `ALLOCSET_DEFAULT_MAXSIZE`.
const DEFAULT_MAX_BLOCK: usize = 8 * 1024 * 1024;
/// `MAXALIGN(sizeof(BumpContext))`.
const BUMP_CONTEXT: usize = 112;
/// bump.c's `Bump_BLOCKHDRSZ`.
const BUMP_BLOCK_HEADER: usize = 32;

/// `GetMemoryChunkSpace` of an aset.c chunk of `size` requested bytes: a
/// power of two of at least 8 bytes, or a dedicated block past the chunk
/// limit, with its header.
pub(crate) const fn allocset_chunk_space(size: usize) -> usize {
    if size > ALLOC_CHUNK_LIMIT {
        maxalign(size) + CHUNK_HEADER
    } else if size <= 8 {
        8 + CHUNK_HEADER
    } else {
        size.next_power_of_two() + CHUNK_HEADER
    }
}

/// `GetMemoryChunkSpace` of a generation.c chunk of `size` bytes.
pub(crate) const fn generation_chunk_space(size: usize) -> usize {
    maxalign(size) + CHUNK_HEADER
}

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

/// The physical layout of a value's type, from the value itself; `None`
/// for values whose type pos3ql's datum does not determine (a `text`-like
/// datum may be `name`) or whose stored size it does not model.
fn datum_layout(value: &Datum<'_>) -> Option<PhysicalLayout> {
    let oid = match value {
        Datum::Bool(_) => 16,
        Datum::Char(_) => 18,
        Datum::Int8(_) => 20,
        Datum::Int2(_) => 21,
        Datum::Int4(_) => 23,
        Datum::Text(_) => 25,
        Datum::Oid(_) => 26,
        Datum::Tid(_) => 27,
        Datum::Float4(_) => 700,
        Datum::Float8(_) => 701,
        Datum::Money(_) => 790,
        Datum::Bpchar(_) => 1042,
        Datum::Date(_) => 1082,
        Datum::Time(_) => 1083,
        Datum::Timestamp(_) => 1114,
        Datum::Timestamptz(_) => 1184,
        Datum::Interval(_) => 1186,
        Datum::Timetz(..) => 1266,
        Datum::Numeric(_) => 1700,
        Datum::Uuid(_) => 2950,
        Datum::Bytea(_) => 17,
        _ => return None,
    };
    crate::sql::type_layouts::builtin(oid)
}

/// The bytes of a varlena value's data after its header, as PostgreSQL
/// stores it uncompressed; `None` for types sized differently from pos3ql.
pub(crate) fn varlena_payload(value: &Datum<'_>) -> Option<usize> {
    use crate::sql::numeric::Sign;
    Some(match value {
        Datum::Text(text) | Datum::Bpchar(text) | Datum::Xml(text) => text.len(),
        Datum::Json { text, jsonb: false } => text.len(),
        Datum::Bytea(bytes) => bytes.len(),
        // `NumericShort` (a two-byte header) when the weight and display
        // scale fit, else the long form, then base-10000 digits.
        Datum::Numeric(value) => match value.sign {
            Sign::Pos | Sign::Neg => {
                let short = value.dscale <= 63 && (-64..=63).contains(&value.weight);
                (if short { 2 } else { 4 }) + 2 * value.ndigits()
            }
            Sign::NaN | Sign::PosInf | Sign::NegInf => 2,
        },
        _ => return None,
    })
}

/// Adds one non-null value of `layout` to a tuple's data of `data` bytes
/// (`heap_compute_data_size`): a packable varlena short enough takes a
/// one-byte header and no alignment.
pub(crate) fn add_value(data: usize, layout: PhysicalLayout, value: &Datum<'_>) -> Option<usize> {
    if layout.length > 0 {
        return Some(align_to(data, layout.alignment) + layout.length as usize);
    }
    if layout.length != -1 {
        return None;
    }
    let payload = varlena_payload(value)?;
    Some(if layout.storage != b'p' && payload < SHORT_VARLENA_MAX {
        data + payload + 1
    } else {
        align_to(data, layout.alignment) + payload + 4
    })
}

/// The `t_len` of the minimal tuple (`heap_form_minimal_tuple`) of `values`,
/// each sized by its own type, followed by `tids` row identities (`ctid`
/// junk columns a row lock adds); `None` when a value has no PostgreSQL size
/// here.
pub(crate) fn minimal_tuple_len_with_tids<'a>(
    values: impl Iterator<Item = Datum<'a>>,
    tids: usize,
) -> Option<usize> {
    let mut data = 0usize;
    let mut columns = 0usize;
    let mut has_null = false;
    for value in values {
        columns += 1;
        if value.is_null() {
            has_null = true;
            continue;
        }
        data = add_value(data, datum_layout(&value)?, &value)?;
    }
    for _ in 0..tids {
        columns += 1;
        data = align_to(data, b's') + 6;
    }
    let bitmap = if has_null { columns.div_ceil(8) } else { 0 };
    Some(maxalign(MINIMAL_TUPLE_HEADER + bitmap) + data)
}

/// The `t_len` of the minimal tuple of a projected row's values at
/// `columns` (every value when `None`).
pub(crate) fn projected_tuple_len(row: &[u8], columns: Option<&[usize]>) -> Option<usize> {
    projected_tuple_len_with_tids(row, columns, 0)
}

/// [`projected_tuple_len`] with `tids` row identities after the values.
pub(crate) fn projected_tuple_len_with_tids(
    row: &[u8],
    columns: Option<&[usize]>,
    tids: usize,
) -> Option<usize> {
    if let Some(columns) = columns {
        let values = columns
            .iter()
            .map(|&column| crate::sql::exec::decode_projected_pub(row, column));
        return minimal_tuple_len_with_tids(values, tids);
    }
    let width = crate::sql::exec::projected_row_width(row);
    let mut at = 2usize;
    let values = (0..width).map(|_| {
        let tag = row[at];
        at += 1;
        let (decoded, size) = crate::sql::exec::decode_projected_value(row, tag, at);
        at += size;
        decoded
    });
    minimal_tuple_len_with_tids(values, tids)
}

/// `work_mem` in bytes, the memory a sort or tuplestore may use.
fn work_mem_bytes() -> i64 {
    i64::from(crate::sql::guc::work_mem_kilobytes()).saturating_mul(1024)
}

/// `sizeof(SortTuple)`.
const SORT_TUPLE: usize = 24;
/// `INITIAL_MEMTUPSIZE`.
const INITIAL_SORT_SLOTS: usize = 1024;

/// How a tuplesort copies the rows it is given.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum SortTuples {
    /// A sort of one column (`nodeSort.c`'s datum sort), which charges
    /// nothing per row.
    Datum,
    /// Rows copied into a bump context: each row's maxaligned size.
    Bump,
    /// Rows copied into an aset.c context, as a sort a `Limit` may bound
    /// keeps them: each row's chunk.
    AllocSet,
}

/// The memory an in-memory tuplesort reports (`tuplesort_get_stats` after
/// `tuplesort_performsort`): its `SortTuple` array, grown as
/// `grow_memtuples` grows it while the rows arrived, and the rows it still
/// charges. `input` is the `t_len` of every row it read, in arrival order;
/// `heap_charge` what the rows charge once a bounded heap took them (see
/// [`bounded_heap_charge`]). `None` when the rows overflowed `work_mem`,
/// which spills them.
pub(crate) fn tuplesort_memory(
    tuples: SortTuples,
    input: impl Iterator<Item = usize>,
    bound: Option<usize>,
    heap_charge: Option<usize>,
) -> Option<usize> {
    let charge = |length: usize| match tuples {
        SortTuples::Datum => 0,
        SortTuples::Bump => maxalign(length),
        SortTuples::AllocSet => allocset_chunk_space(length),
    };
    let allowed = work_mem_bytes();
    let array_space = |slots: usize| (maxalign(slots * SORT_TUPLE) + CHUNK_HEADER) as i64;
    let mut slots = INITIAL_SORT_SLOTS;
    let mut available = allowed - array_space(slots);
    let mut grows = true;
    let mut count = 0usize;
    let mut heap = false;
    for length in input {
        available -= charge(length) as i64;
        if heap {
            continue;
        }
        if count + 1 >= slots && grows {
            let used = allowed - available;
            let next = if used <= available {
                slots * 2
            } else {
                grows = false;
                (slots as f64 * (allowed as f64 / used as f64)) as usize
            };
            if next > slots && available >= ((next - slots) * SORT_TUPLE) as i64 {
                available += array_space(slots) - array_space(next);
                slots = next;
            } else {
                grows = false;
            }
        }
        count += 1;
        if let Some(bound) = bound
            && (count > bound * 2 || (count > bound && available < 0))
        {
            heap = true;
            continue;
        }
        if count >= slots || available < 0 {
            return None;
        }
    }
    let rows = if heap {
        heap_charge.unwrap_or(0)
    } else {
        (allowed - array_space(slots) - available) as usize
    };
    Some(array_space(slots) as usize + rows)
}

/// What the rows of a bounded sort charge once its heap took them
/// (`make_bounded_heap`, then `TSS_BOUNDED`): the heap keeps the `bound`
/// best rows of the first `2 * bound + 1` and frees each row no better
/// than its worst, but a row that replaces the worst there leaves that one
/// charged; past them a row better than the worst frees it. `chunks` are
/// the rows' charges in arrival order and `better(new, worst)` whether a
/// row sorts strictly before another; `heap` holds `bound` positions.
pub(crate) fn bounded_heap_charge<E>(
    chunks: &[usize],
    bound: usize,
    heap: &mut [usize],
    mut better: impl FnMut(usize, usize) -> Result<bool, E>,
) -> Result<usize, E> {
    let heapify_at = (bound * 2 + 1).min(chunks.len());
    let mut charge: usize = chunks[..heapify_at].iter().sum();
    let mut size = 0usize;
    // A heap whose top is its worst row: `worse(a, b)` when `b` is better.
    let sift_down = |heap: &mut [usize],
                         size: usize,
                         better: &mut dyn FnMut(usize, usize) -> Result<bool, E>|
     -> Result<(), E> {
        let mut at = 0;
        loop {
            let (left, right) = (2 * at + 1, 2 * at + 2);
            let mut worst = at;
            if left < size && better(heap[worst], heap[left])? {
                worst = left;
            }
            if right < size && better(heap[worst], heap[right])? {
                worst = right;
            }
            if worst == at {
                return Ok(());
            }
            heap.swap(at, worst);
            at = worst;
        }
    };
    for row in 0..chunks.len() {
        if row >= heapify_at {
            charge += chunks[row];
        }
        if size < bound {
            heap[size] = row;
            let mut at = size;
            size += 1;
            while at > 0 {
                let parent = (at - 1) / 2;
                if better(heap[parent], heap[at])? {
                    heap.swap(parent, at);
                    at = parent;
                } else {
                    break;
                }
            }
            continue;
        }
        if !better(row, heap[0])? {
            charge -= chunks[row];
            continue;
        }
        if row >= heapify_at {
            charge -= chunks[heap[0]];
        }
        heap[0] = row;
        sift_down(heap, size, &mut better)?;
    }
    Ok(charge)
}

/// `memtupsize`'s first value in a tuplestore.
const INITIAL_STORE_SLOTS: usize = 2048;

/// A tuplestore's memory as `tuplestore.c` accounts it: its row pointer
/// array and each row's generation chunk, and the most it held when
/// `tuplestore_updatemax` looked (at a clear, a trim and the end).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Tuplestore {
    allowed: i64,
    available: i64,
    slots: usize,
    count: usize,
    grows: bool,
    most: i64,
    /// The bytes written once the rows overflowed `work_mem`.
    file: Option<i64>,
}

impl Tuplestore {
    pub(crate) fn new() -> Self {
        let allowed = work_mem_bytes();
        Self {
            allowed,
            available: allowed - Self::array_space(INITIAL_STORE_SLOTS),
            slots: INITIAL_STORE_SLOTS,
            count: 0,
            grows: true,
            most: 0,
            file: None,
        }
    }

    const fn array_space(slots: usize) -> i64 {
        (maxalign(slots * 8) + CHUNK_HEADER) as i64
    }

    fn update_most(&mut self) {
        let used = match self.file {
            Some(bytes) => bytes,
            None => self.allowed - self.available,
        };
        self.most = self.most.max(used);
    }

    /// Stores a row whose minimal tuple is `length` bytes
    /// (`tuplestore_puttupleslot`).
    pub(crate) fn put(&mut self, length: usize) {
        if let Some(bytes) = &mut self.file {
            // `writetup_heap`: the length word and the tuple after its
            // `MINIMAL_TUPLE_DATA_OFFSET` bytes.
            *bytes += (length - 10 + 4) as i64;
            return;
        }
        self.available -= generation_chunk_space(length) as i64;
        if self.count + 1 >= self.slots && self.grows {
            let used = self.allowed - self.available;
            let next = if used <= self.available {
                self.slots * 2
            } else {
                self.grows = false;
                (self.slots as f64 * (self.allowed as f64 / used as f64)) as usize
            };
            if next > self.slots && self.available >= ((next - self.slots) * 8) as i64 {
                self.available += Self::array_space(self.slots) - Self::array_space(next);
                self.slots = next;
            } else {
                self.grows = false;
            }
        }
        self.count += 1;
        if self.count < self.slots && self.available >= 0 {
            return;
        }
        self.update_most();
        self.file = Some(0);
        self.count = 0;
    }

    /// Drops every row and keeps the array (`tuplestore_clear`).
    pub(crate) fn clear(&mut self) {
        self.update_most();
        self.file = None;
        self.available = self.allowed - Self::array_space(self.slots);
        self.count = 0;
    }

    /// The figures `EXPLAIN` prints: whether the rows went to disk and the
    /// most bytes used.
    pub(crate) fn finish(mut self) -> (bool, usize) {
        self.update_most();
        (self.file.is_some(), self.most as usize)
    }
}

/// `sizeof(TupleHashEntryData)`.
const TUPLE_HASH_ENTRY: usize = 16;
/// `sizeof(AggStatePerGroupData)`.
const PER_GROUP_STATE: usize = 16;
/// The most by-reference transition states a modelled hash aggregate keeps.
pub(crate) const MAX_SIZED_STATES: usize = 8;

/// What sizes a hash aggregate's memory, decided when it is planned.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct HashAggSizing {
    /// The buckets `build_hash_table` asks for.
    pub(crate) buckets: usize,
    /// The grouping columns the table hashes.
    pub(crate) columns: usize,
    /// The transition states (`numtrans`).
    pub(crate) transitions: usize,
    /// The bytes each group's by-reference transition states allocate
    /// when the group starts.
    pub(crate) per_group: [u16; MAX_SIZED_STATES],
    pub(crate) per_group_count: usize,
}

/// What `BuildTupleHashTable` and the expressions and expression context
/// it makes take from the meta context's first block besides the bucket
/// array, by grouping columns: the table and `simplehash` headers, then
/// the descriptor copy, slot, hash and equality expressions (whose step
/// array grows past three columns), measured on PostgreSQL 18.6
/// (`pg_log_backend_memory_contexts` before the first group).
const META_HEADERS: usize = 136 + 72;
const META_REST: [usize; 7] = [3288, 3656, 4128, 6400, 7128, 7480, 7696];

impl HashAggSizing {
    /// The most grouping columns the meta context model covers.
    pub(crate) const MAX_COLUMNS: usize = META_REST.len();

    /// `build_hash_tables`' buckets for a table of `groups` estimated groups
    /// whose entries `hash_agg_entry_size` estimates at `entry_size`:
    /// `hash_choose_num_buckets` halves what `hash_mem` holds, and
    /// `BuildTupleHashTable` caps it at what `hash_mem` holds of bare
    /// entries.
    pub(crate) fn buckets(entry_size: f64, groups: f64, memory: f64, transitions: usize) -> usize {
        let most = ((memory / entry_size) as i64) >> 1;
        let buckets = (groups as i64).min(most).max(1);
        let entry = (TUPLE_HASH_ENTRY + maxalign(transitions * PER_GROUP_STATE)) as i64;
        let limit = (work_mem_bytes() * 2) / entry;
        usize::try_from(buckets.min(limit)).unwrap_or(1)
    }
}

/// The memory a hash aggregate reports (`hash_agg_update_metrics` when its
/// input ends): its meta context with the bucket array, the bump context of
/// its entries and the context of its by-reference transition values.
#[derive(Clone, Copy, Debug)]
pub(crate) struct HashAggMemory {
    sizing: HashAggSizing,
    /// `simplehash`'s bucket count and entries.
    buckets: usize,
    entries: usize,
    /// The bump context's allocated blocks and the space left in the
    /// current one.
    entry_blocks: usize,
    entry_free: usize,
    entry_next_block: usize,
    entry_max_block: usize,
    /// The meta context's blocks besides a dedicated bucket array's.
    meta: AllocSetBlocks,
    /// The transition values' context.
    transition: AllocSetBlocks,
}

/// The blocks an aset.c context with the default first block size
/// allocates for chunks that are never freed.
#[derive(Clone, Copy, Debug)]
pub(crate) struct AllocSetBlocks {
    allocated: usize,
    free: usize,
    next_block: usize,
    max_block: usize,
}

impl AllocSetBlocks {
    const fn new(max_block: usize) -> Self {
        Self {
            allocated: DEFAULT_INIT_BLOCK,
            free: DEFAULT_INIT_BLOCK - ALLOC_SET_CONTEXT - ALLOC_BLOCK_HEADER,
            next_block: DEFAULT_INIT_BLOCK,
            max_block,
        }
    }

    /// Takes `bytes` of chunks from the current block, or a new one.
    fn take(&mut self, bytes: usize) {
        if bytes > self.free {
            // The rest of the block goes to the free lists; the new block
            // is the next size, doubled up to the maximum, or larger for
            // the chunk.
            let mut block = self.next_block;
            self.next_block = (self.next_block * 2).min(self.max_block);
            while block < bytes + ALLOC_BLOCK_HEADER {
                block *= 2;
            }
            self.allocated += block;
            self.free = block - ALLOC_BLOCK_HEADER;
        }
        self.free -= bytes;
    }

    /// Allocates `size` bytes (`AllocSetAlloc`).
    pub(crate) fn alloc(&mut self, size: usize) {
        if size > ALLOC_CHUNK_LIMIT {
            self.allocated += maxalign(size) + ALLOC_BLOCK_HEADER + CHUNK_HEADER;
            return;
        }
        self.take(allocset_chunk_space(size));
    }
}

/// `pg_prevpower2_size_t(work_mem * 1024 / 16)` clamped to the default
/// block sizes: the largest block of the contexts sized by `work_mem`.
fn work_mem_block() -> usize {
    let bytes = usize::try_from(work_mem_bytes()).unwrap_or(0) / 16;
    let previous = if bytes == 0 {
        0
    } else {
        1usize << bytes.ilog2()
    };
    previous.clamp(DEFAULT_INIT_BLOCK, DEFAULT_MAX_BLOCK)
}

/// `SH_FILLFACTOR`.
const FILL_FACTOR: f64 = 0.9;

/// `simplehash`'s bucket count for `elements` (`SH_CREATE`).
fn simplehash_size(elements: usize) -> usize {
    let size = (elements as f64 / FILL_FACTOR) as u64;
    usize::try_from(size.max(2).next_power_of_two()).unwrap_or(usize::MAX)
}

impl HashAggMemory {
    /// The contexts once `build_hash_table` made the table; `None` past
    /// the grouping columns the model covers.
    pub(crate) fn new(sizing: HashAggSizing) -> Option<Self> {
        let rest = *META_REST.get(sizing.columns.checked_sub(1)?)?;
        let mut memory = Self {
            sizing,
            buckets: 0,
            entries: 0,
            entry_blocks: DEFAULT_INIT_BLOCK,
            entry_free: DEFAULT_INIT_BLOCK - BUMP_CONTEXT - BUMP_BLOCK_HEADER,
            entry_next_block: DEFAULT_INIT_BLOCK,
            entry_max_block: work_mem_block(),
            meta: AllocSetBlocks::new(DEFAULT_MAX_BLOCK),
            transition: AllocSetBlocks::new(work_mem_block()),
        };
        memory.meta.take(META_HEADERS);
        memory.allocate_buckets(simplehash_size(sizing.buckets));
        memory.meta.take(rest);
        Some(memory)
    }

    fn allocate_buckets(&mut self, size: usize) {
        let bytes = size * TUPLE_HASH_ENTRY;
        if bytes <= ALLOC_CHUNK_LIMIT {
            self.meta.take(allocset_chunk_space(bytes));
        }
        self.buckets = size;
    }

    /// A dedicated block holding the current bucket array, when it is past
    /// the chunk limit; an older one was freed when the table grew.
    fn bucket_block(&self) -> usize {
        let bytes = self.buckets * TUPLE_HASH_ENTRY;
        if bytes > ALLOC_CHUNK_LIMIT {
            maxalign(bytes) + ALLOC_BLOCK_HEADER + CHUNK_HEADER
        } else {
            0
        }
    }

    /// Looks an input row up (`SH_INSERT`, which grows the table when its
    /// entries reached its fill factor, whether or not the key is new);
    /// `new` is the `t_len` of the representative tuple of a new group.
    pub(crate) fn lookup(&mut self, new: Option<usize>) {
        let threshold = (self.buckets as f64 * FILL_FACTOR) as usize;
        if self.entries >= threshold {
            self.allocate_buckets(self.buckets * 2);
        }
        let Some(length) = new else {
            return;
        };
        self.entries += 1;
        let chunk = maxalign(length + maxalign(self.sizing.transitions * PER_GROUP_STATE));
        if chunk > self.entry_max_block {
            self.entry_blocks += chunk + BUMP_BLOCK_HEADER;
        } else {
            if chunk > self.entry_free {
                let mut block = self.entry_next_block;
                self.entry_next_block = (self.entry_next_block * 2).min(self.entry_max_block);
                if block < chunk + BUMP_BLOCK_HEADER {
                    block = (chunk + BUMP_BLOCK_HEADER).next_power_of_two();
                }
                self.entry_blocks += block;
                self.entry_free = block - BUMP_BLOCK_HEADER;
            }
            self.entry_free -= chunk;
        }
        for &bytes in &self.sizing.per_group[..self.sizing.per_group_count] {
            self.transition.alloc(usize::from(bytes));
        }
    }

    /// The bytes `hash_mem_peak` holds once the input ended: the meta
    /// context (with the expression context `BuildTupleHashTable` creates
    /// under it), the entries and the transition values.
    pub(crate) fn peak(&self) -> usize {
        self.meta.allocated
            + self.bucket_block()
            + DEFAULT_INIT_BLOCK
            + self.entry_blocks
            + self.transition.allocated
    }
}

#[cfg(all(test, not(feature = "sql-fixture-suite")))]
mod tests {
    use super::*;

    #[test]
    fn sorts_charge_postgresql_chunks() {
        // 100 rows of two integers: 24584 + 100 * 24 bytes, 27kB.
        let lengths = core::iter::repeat_n(24, 100);
        assert_eq!(
            tuplesort_memory(SortTuples::Bump, lengths, None, None),
            Some(26_984)
        );
        // Ascending input to a descending bounded sort: each row of the
        // first seven replaces the heap's worst, which stays charged.
        let chunks = [40usize; 100];
        let mut heap = [0usize; 3];
        let charge =
            bounded_heap_charge::<()>(&chunks, 3, &mut heap, |new, worst| Ok(new > worst)).unwrap();
        assert_eq!(charge, 7 * 40);
        assert_eq!(
            tuplesort_memory(
                SortTuples::AllocSet,
                core::iter::repeat_n(24, 100),
                Some(3),
                Some(charge)
            ),
            Some(24_584 + 280)
        );
        assert_eq!(
            tuplesort_memory(SortTuples::Datum, core::iter::repeat_n(24, 120), None, None),
            Some(24_584)
        );
    }

    #[test]
    fn tuplestores_grow_their_array_at_the_last_slot() {
        let mut store = Tuplestore::new();
        for _ in 0..2076 {
            store.put(20);
        }
        // 4096 slots and 2076 rows of 32 bytes: 97kB.
        assert_eq!(store.finish(), (false, 32_776 + 2076 * 32));
        let mut store = Tuplestore::new();
        for _ in 0..30 {
            store.put(20);
        }
        assert_eq!(store.finish(), (false, 16_392 + 30 * 32));
    }
}
