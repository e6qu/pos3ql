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
        Datum::Cidr(_) => 650,
        Datum::Macaddr8(_) => 774,
        Datum::Macaddr(_) => 829,
        Datum::Inet(_) => 869,
        Datum::PgLsn(_) => 3220,
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
        // `inet_struct`: family and mask length, then the address bytes.
        Datum::Inet(address) | Datum::Cidr(address) => 2 + address.addr_len(),
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

/// `datumGetSize` of the value a datum sort copies: nothing for a null,
/// a pass-by-value datum's eight bytes, a fixed-width value's length, or a
/// varlena as stored (a one-byte header when short). `column` names the
/// value in a projected row (the first when `None`).
pub(crate) fn sorted_datum_size(row: &[u8], column: Option<usize>) -> usize {
    let value = crate::sql::exec::decode_projected_pub(row, column.unwrap_or(0));
    if value.is_null() {
        return 0;
    }
    let Some(layout) = datum_layout(&value) else {
        return 0;
    };
    if layout.by_value {
        return core::mem::size_of::<usize>();
    }
    if layout.length > 0 {
        return layout.length as usize;
    }
    match varlena_payload(&value) {
        Some(payload) if layout.storage != b'p' && payload < SHORT_VARLENA_MAX => payload + 1,
        Some(payload) => payload + 4,
        None => 0,
    }
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

/// `BLCKSZ`: the size of a temporary file block and of a tape buffer.
const BLOCK_BYTES: u64 = 8192;
/// `TapeBlockPayloadSize`: a tape block less its two-`int64` trailer.
const TAPE_BLOCK_PAYLOAD: u64 = BLOCK_BYTES - 16;
/// `TAPE_BUFFER_OVERHEAD`.
const TAPE_BUFFER_OVERHEAD: i64 = BLOCK_BYTES as i64;
/// `MERGE_BUFFER_SIZE`.
const MERGE_BUFFER_SIZE: i64 = 32 * BLOCK_BYTES as i64;
/// `MINORDER` and `MAXORDER`.
const MIN_MERGE_ORDER: usize = 6;
const MAX_MERGE_ORDER: usize = 500;

/// `tuplesort_merge_order`: the tapes a sort of `allowed` bytes merges.
pub(crate) fn tuplesort_merge_order(allowed: i64) -> usize {
    let order = allowed / (2 * TAPE_BUFFER_OVERHEAD + MERGE_BUFFER_SIZE);
    usize::try_from(order)
        .unwrap_or(0)
        .clamp(MIN_MERGE_ORDER, MAX_MERGE_ORDER)
}

/// What a tuplesort reports (`tuplesort_get_stats`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum TuplesortSpace {
    /// The rows were sorted in memory, which held this many bytes.
    Memory(usize),
    /// The rows were written as runs on tapes and merged: the bytes of the
    /// temporary file (`LogicalTapeSetBlocks`). A merge pass reads a block
    /// before it writes the block that replaces it, so the file never grows
    /// past the initial runs' blocks, however many passes the merge takes.
    Disk(usize),
}

/// One sort's accounting as `tuplesort.c` keeps it (`availMem` and its
/// `memtuples` array) and, once the rows overflow `work_mem`, the runs it
/// writes to tapes (`inittapes`, `dumptuples`). Feed it each row in arrival
/// order, then ask for [`TuplesortModel::space`]. It holds a fixed amount:
/// the tapes are an array of the most `MAXORDER` byte counts.
#[derive(Clone, Copy, Debug)]
pub(crate) struct TuplesortModel {
    tuples: SortTuples,
    bound: Option<usize>,
    allowed: i64,
    available: i64,
    slots: usize,
    grows: bool,
    count: usize,
    /// `make_bounded_heap` ran, once this many rows had arrived.
    heap: bool,
    heap_at: usize,
    /// The charge of the rows since the last run, which a dump returns.
    tuple_memory: i64,
    /// The tape bytes of the rows since the last run.
    run_bytes: u64,
    /// `inittapes` ran: the tapes' count and what each holds.
    tapes: Option<Tapes>,
}

#[derive(Clone, Copy, Debug)]
struct Tapes {
    max_tapes: usize,
    runs: usize,
    /// The bytes written to each tape, run end markers included.
    tape_bytes: [u64; MAX_MERGE_ORDER],
}

impl TuplesortModel {
    /// A sort of the session's `work_mem`; `bound` is the row count a limit
    /// lets it keep (`tuplesort_set_bound`).
    pub(crate) fn new(tuples: SortTuples, bound: Option<usize>) -> Self {
        let allowed = work_mem_bytes();
        Self {
            tuples,
            bound,
            allowed,
            available: allowed - Self::array_space(INITIAL_SORT_SLOTS),
            slots: INITIAL_SORT_SLOTS,
            grows: true,
            count: 0,
            heap: false,
            heap_at: 0,
            tuple_memory: 0,
            run_bytes: 0,
            tapes: None,
        }
    }

    fn array_space(slots: usize) -> i64 {
        (maxalign(slots * SORT_TUPLE) + CHUNK_HEADER) as i64
    }

    /// Whether the rows went to a bounded heap, whose memory the caller
    /// charges (see [`bounded_heap_charge`]).
    pub(crate) fn bounded(&self) -> bool {
        self.heap
    }

    /// The rows `make_bounded_heap` took in at once.
    pub(crate) fn heap_at(&self) -> usize {
        self.heap_at
    }

    /// `grow_memtuples`.
    fn grow(&mut self) {
        let used = self.allowed - self.available;
        let next = if used <= self.available {
            self.slots * 2
        } else {
            self.grows = false;
            (self.slots as f64 * (self.allowed as f64 / used as f64)) as usize
        };
        if next > self.slots && self.available >= ((next - self.slots) * SORT_TUPLE) as i64 {
            self.available += Self::array_space(self.slots) - Self::array_space(next);
            self.slots = next;
        } else {
            self.grows = false;
        }
    }

    /// `tuplesort_puttuple_common` of a row whose copy is `length` bytes (the
    /// minimal tuple's `t_len`, or a datum's size) and whose tape image is
    /// `written` bytes (`writetup_heap`, `writetup_datum`).
    pub(crate) fn push(&mut self, length: usize, written: usize) {
        let charge = match self.tuples {
            SortTuples::Datum => 0,
            SortTuples::Bump => maxalign(length),
            SortTuples::AllocSet => allocset_chunk_space(length),
        } as i64;
        self.available -= charge;
        self.tuple_memory += charge;
        self.run_bytes += written as u64;
        if self.tapes.is_some() {
            self.count += 1;
            if self.count >= self.slots || self.available < 0 {
                self.dump();
            }
            return;
        }
        if self.heap {
            return;
        }
        if self.count + 1 >= self.slots && self.grows {
            self.grow();
        }
        self.count += 1;
        if let Some(bound) = self.bound
            && (self.count > bound * 2 || (self.count > bound && self.available < 0))
        {
            self.heap = true;
            self.heap_at = self.count;
            return;
        }
        if self.count >= self.slots || self.available < 0 {
            self.begin_tapes();
            self.dump();
        }
    }

    /// `inittapes` (and `inittapestate`'s tape buffers).
    fn begin_tapes(&mut self) {
        let max_tapes = tuplesort_merge_order(self.allowed);
        let space = max_tapes as i64 * TAPE_BUFFER_OVERHEAD;
        if space + Self::array_space(self.slots) < self.allowed {
            self.available -= space;
        }
        self.tapes = Some(Tapes {
            max_tapes,
            runs: 0,
            tape_bytes: [0; MAX_MERGE_ORDER],
        });
    }

    /// `dumptuples`: the rows in memory become one run, on the tape the runs
    /// take in turn, ended by a zero length word.
    fn dump(&mut self) {
        let tapes = self.tapes.as_mut().expect("tapes begun");
        tapes.tape_bytes[tapes.runs % tapes.max_tapes] += self.run_bytes + 4;
        tapes.runs += 1;
        self.run_bytes = 0;
        self.count = 0;
        self.available += self.tuple_memory;
        self.tuple_memory = 0;
    }

    /// What `tuplesort_get_stats` reports once the input ended
    /// (`tuplesort_performsort`'s last `dumptuples` included); `heap_charge`
    /// is what the rows of a bounded heap charge, which only a sort that
    /// took one needs.
    pub(crate) fn space(mut self, heap_charge: Option<usize>) -> TuplesortSpace {
        if self.tapes.is_some() {
            if self.count > 0 {
                self.dump();
            }
            let tapes = self.tapes.as_ref().expect("tapes begun");
            let blocks: u64 = tapes
                .tape_bytes
                .iter()
                .map(|&bytes| bytes.div_ceil(TAPE_BLOCK_PAYLOAD))
                .sum();
            return TuplesortSpace::Disk((blocks * BLOCK_BYTES) as usize);
        }
        let array = Self::array_space(self.slots);
        let rows = if self.heap {
            heap_charge.unwrap_or(0) as i64
        } else {
            self.allowed - array - self.available
        };
        TuplesortSpace::Memory((array + rows) as usize)
    }
}

/// The report of a sort whose rows (`input`: each one's copy length and tape
/// image bytes, in arrival order) a sort of `bound` rows (if any) read.
pub(crate) fn tuplesort_space(
    tuples: SortTuples,
    input: impl Iterator<Item = (usize, usize)>,
    bound: Option<usize>,
    heap_charge: Option<usize>,
) -> TuplesortSpace {
    let mut model = TuplesortModel::new(tuples, bound);
    for (length, written) in input {
        model.push(length, written);
    }
    model.space(heap_charge)
}

/// What the rows of a bounded sort charge once its heap took them
/// (`make_bounded_heap`, then `TSS_BOUNDED`): the heap keeps the `bound`
/// best rows of the first `2 * bound + 1` and frees each row no better
/// than its worst, but a row that replaces the worst there leaves that one
/// charged; past them a row better than the worst frees it. `chunk` gives
/// the rows' charges in arrival order and `better(new, worst)` whether a
/// row sorts strictly before another; `heap` holds the bound's positions.
pub(crate) fn bounded_heap_charge<E>(
    count: usize,
    heapify_at: usize,
    chunk: impl Fn(usize) -> usize,
    heap: &mut [usize],
    mut better: impl FnMut(usize, usize) -> Result<bool, E>,
) -> Result<usize, E> {
    let bound = heap.len();
    let heapify_at = heapify_at.min(count);
    let mut charge: usize = (0..heapify_at).map(&chunk).sum();
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
    for row in 0..count {
        if row >= heapify_at {
            charge += chunk(row);
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
            charge -= chunk(row);
            continue;
        }
        if row >= heapify_at {
            charge -= chunk(heap[0]);
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
        let limit = crate::sql::guc::hash_memory_bytes().min(i64::MAX as f64) as i64 / entry;
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
            tuplesort_space(SortTuples::Bump, lengths.map(|n| (n, 0)), None, None),
            TuplesortSpace::Memory(26_984)
        );
        // Ascending input to a descending bounded sort: each row of the
        // first seven replaces the heap's worst, which stays charged.
        let mut heap = [0usize; 3];
        let charge =
            bounded_heap_charge::<()>(100, 7, |_| 40, &mut heap, |new, worst| Ok(new > worst))
                .unwrap();
        assert_eq!(charge, 7 * 40);
        assert_eq!(
            tuplesort_space(
                SortTuples::AllocSet,
                core::iter::repeat_n((24, 0), 100),
                Some(3),
                Some(charge)
            ),
            TuplesortSpace::Memory(24_584 + 280)
        );
        assert_eq!(
            tuplesort_space(
                SortTuples::Datum,
                core::iter::repeat_n((24, 0), 120),
                None,
                None
            ),
            TuplesortSpace::Memory(24_584)
        );
    }

    #[test]
    fn sorts_past_work_mem_write_tapes() {
        // 4MB of work_mem: 100000 rows of 100 bytes overflow into runs, each
        // tape holding its runs' length words, tuples and end markers in
        // 8176-byte blocks.
        let space = tuplesort_space(
            SortTuples::Bump,
            core::iter::repeat_n((100, 94), 100_000),
            None,
            None,
        );
        let TuplesortSpace::Disk(bytes) = space else {
            panic!("expected a spill, got {space:?}");
        };
        let payload: usize = 100_000 * 94;
        assert!(bytes >= payload.div_ceil(8176) * 8192);
        assert_eq!(bytes % 8192, 0);
        assert_eq!(tuplesort_merge_order(4 << 20), 15);
        assert_eq!(tuplesort_merge_order(64 << 10), 6);
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
