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

/// What sizing a value may need beyond the value: the catalog that decodes a
/// stored named composite and scratch memory for a parse.
pub(crate) struct Sizing<'s> {
    /// The storage and transaction whose catalog names a composite type's
    /// fields; none where only built-in values are sized.
    pub(crate) catalog: Option<(&'s crate::storage::Storage, u32)>,
    pub(crate) scratch: &'s crate::mem::arena::Arena,
}

/// Where a sort's values are sized from: the storage whose catalog names the
/// composite types, as of the transaction.
#[derive(Clone, Copy)]
pub(crate) struct SizeSource<'s> {
    pub(crate) storage: &'s crate::storage::Storage,
    pub(crate) txid: u32,
}

impl SizeSource<'_> {
    /// Runs `size` with the scratch memory the catalog's decoding uses.
    pub(crate) fn with<R>(self, size: impl FnOnce(&Sizing<'_>) -> R) -> R {
        self.storage.with_comparison_scratch(|scratch| {
            size(&Sizing {
                catalog: Some((self.storage, self.txid)),
                scratch,
            })
        })
    }
}

/// A non-null value as a tuple stores it: its type's layout and, for a
/// varlena, the data bytes after its header.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Stored {
    pub(crate) layout: PhysicalLayout,
    pub(crate) payload: usize,
}

/// The layout of the type a value's variant names; `None` for a value whose
/// type its datum does not determine or whose stored size is not modelled.
fn datum_layout(value: &Datum<'_>) -> Option<PhysicalLayout> {
    use crate::sql::types::{ArrElem, ColType, GeometryKind, RangeKind};
    let oid = match value {
        Datum::Bool(_) => 16,
        Datum::Char(_) => 18,
        Datum::Int8(_) => 20,
        Datum::Int2(_) => 21,
        Datum::Int4(_) => 23,
        Datum::Text(_) => 25,
        Datum::Oid(_) => 26,
        Datum::Tid(_) => 27,
        Datum::Cid(_) => 29,
        Datum::Int2Vector { .. } => 22,
        Datum::OidVector { .. } => 30,
        Datum::Xml(_) => 142,
        Datum::PgNodeTree(_) => 194,
        Datum::Json { jsonb: false, .. } => 114,
        Datum::Json { jsonb: true, .. } => 3802,
        Datum::JsonPath(_) => 4072,
        Datum::TsVector(_) => 3614,
        Datum::Float4(_) => 700,
        Datum::Float8(_) => 701,
        Datum::Money(_) => 790,
        Datum::AclItem(_) => 1033,
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
        Datum::Xid8(_) => 5069,
        Datum::Regtype { .. } => 2206,
        Datum::RegObject { type_oid, .. } => *type_oid,
        Datum::Bit { varying: false, .. } => 1560,
        Datum::Bit { varying: true, .. } => 1562,
        Datum::Geometry { kind, .. } => match kind {
            GeometryKind::Point => 600,
            GeometryKind::Lseg => 601,
            GeometryKind::Path => 602,
            GeometryKind::Box => 603,
            GeometryKind::Polygon => 604,
            GeometryKind::Line => 628,
            GeometryKind::Circle => 718,
        },
        // An enum label is its `oid`.
        Datum::Enum { .. } => 26,
        Datum::Array { element, .. } => {
            if matches!(element, ArrElem::Nested { .. }) {
                return crate::sql::type_layouts::builtin(2277);
            }
            return Some(ColType::Array(*element).physical_layout());
        }
        // A composite datum: `record` or a named composite, `'d'` aligned.
        Datum::Record(_) | Datum::Composite { .. } | Datum::CompositeText { .. } => 2249,
        Datum::TsQuery(_) => 3615,
        Datum::Snapshot { legacy: true, .. } => 2970,
        Datum::Snapshot { legacy: false, .. } => 5038,
        Datum::Range { kind, .. } => match kind {
            RangeKind::Int4 => 3904,
            RangeKind::Num => 3906,
            RangeKind::Ts => 3908,
            RangeKind::Tstz => 3910,
            RangeKind::Date => 3912,
            RangeKind::Int8 => 3926,
        },
        Datum::Multirange { kind, .. } => match kind {
            RangeKind::Int4 => 4451,
            RangeKind::Num => 4532,
            RangeKind::Ts => 4533,
            RangeKind::Tstz => 4534,
            RangeKind::Date => 4535,
            RangeKind::Int8 => 4536,
        },
        _ => return None,
    };
    crate::sql::type_layouts::builtin(oid)
}

/// The bytes of a varlena value's data after its header, as PostgreSQL
/// stores it uncompressed; `None` for types sized differently from pos3ql.
pub(crate) fn varlena_payload(sizing: Option<&Sizing<'_>>, value: &Datum<'_>) -> Option<usize> {
    use super::postgres_values as values;
    use crate::sql::numeric::Sign;
    Some(match value {
        Datum::Text(text) | Datum::Bpchar(text) | Datum::Xml(text) => text.len(),
        Datum::PgNodeTree(node) => node.as_str().len(),
        Datum::Json { text, jsonb: false } => text.len(),
        Datum::Json { text, jsonb: true } => values::jsonb_payload(text)?,
        Datum::Bytea(bytes) => bytes.len(),
        // `inet_struct`: family and mask length, then the address bytes.
        Datum::Inet(address) | Datum::Cidr(address) => 2 + address.addr_len(),
        // `NumericShort` (a two-byte header) when the weight and display
        // scale fit, else the long form, then base-10000 digits.
        Datum::Numeric(value) => match value.sign {
            Sign::Pos | Sign::Neg => {
                // `make_result` strips zero digits at both ends, whatever
                // the arithmetic left in the value.
                let count = value.ndigits();
                let mut first = 0;
                while first < count && value.digit(first) == 0 {
                    first += 1;
                }
                let mut last = count;
                while last > first && value.digit(last - 1) == 0 {
                    last -= 1;
                }
                let weight = if last == first {
                    0
                } else {
                    i64::from(value.weight) - first as i64
                };
                let short = value.dscale <= 63 && (-64..=63).contains(&weight);
                (if short { 2 } else { 4 }) + 2 * (last - first)
            }
            Sign::NaN | Sign::PosInf | Sign::NegInf => 2,
        },
        Datum::Array { element, raw } => values::array_payload(sizing, *element, raw)?,
        Datum::Range { text, kind } => values::range_payload(text, *kind)?,
        Datum::Multirange { text, kind } => values::multirange_payload(text, *kind)?,
        Datum::TsQuery(query) => values::tsquery_payload(query.as_str()),
        Datum::Snapshot { value, .. } => values::snapshot_payload(value.xip().len()),
        Datum::JsonPath(text) => values::jsonpath_payload(text, sizing?.scratch)?,
        Datum::CompositeText {
            slot,
            physical_fields,
            text,
        } => {
            let sizing = sizing?;
            let (storage, txid) = sizing.catalog?;
            let text = sizing.scratch.alloc_str(text).ok()?;
            let decoded = crate::sql::exec::decode_stored_composite_text(
                text,
                *slot,
                *physical_fields,
                storage,
                txid,
                sizing.scratch,
            )
            .ok()?;
            varlena_payload(Some(sizing), &decoded)?
        }
        Datum::TsVector(vector) => {
            crate::storage::statistics::scalar::tsvector_data_length(vector.as_str())
        }
        Datum::Bit { bits, .. } => values::bit_payload(bits),
        Datum::Geometry { kind, text } => match kind {
            crate::sql::types::GeometryKind::Path => values::path_payload(text),
            crate::sql::types::GeometryKind::Polygon => values::polygon_payload(text),
            _ => return None,
        },
        Datum::Int2Vector { raw, .. } => values::vector_payload(raw.len() / 2, 2),
        Datum::OidVector { raw, .. } => values::vector_payload(raw.len() / 4, 4),
        Datum::Record(fields) | Datum::Composite { fields, .. } => {
            composite_length(fields.iter().map(|field| {
                if field.value.is_null() {
                    Some(None)
                } else {
                    stored(sizing, Some(field.type_oid), &field.value).map(Some)
                }
            }))? - 4
        }
        _ => return None,
    })
}

/// The stored form of a non-null `value`; `hint` is its type's OID when the
/// caller knows it (a record's field), which tells a `name` from `text`.
pub(crate) fn stored(
    sizing: Option<&Sizing<'_>>,
    hint: Option<i32>,
    value: &Datum<'_>,
) -> Option<Stored> {
    let layout = hint
        .and_then(crate::sql::type_layouts::builtin)
        .filter(|layout| layout.length != -1 || datum_layout(value).is_some())
        .or_else(|| datum_layout(value))?;
    let payload = if layout.length == -1 {
        varlena_payload(sizing, value)?
    } else {
        0
    };
    Some(Stored { layout, payload })
}

/// What a tuple's data holds so far (`heap_compute_data_size`).
#[derive(Default)]
struct TupleData {
    data: usize,
    columns: usize,
    has_null: bool,
}

impl TupleData {
    /// Adds one column: `None` for NULL, which only sets its bitmap bit.
    fn push(&mut self, value: Option<Stored>) {
        self.columns += 1;
        let Some(Stored { layout, payload }) = value else {
            self.has_null = true;
            return;
        };
        self.data = if layout.length > 0 {
            align_to(self.data, layout.alignment) + layout.length as usize
        } else if layout.storage != b'p' && payload < SHORT_VARLENA_MAX {
            // A packable varlena short enough takes a one-byte header and no
            // alignment.
            self.data + payload + 1
        } else {
            align_to(self.data, layout.alignment) + payload + 4
        };
    }

    /// The tuple's length given its header before the null bitmap.
    fn length(&self, header: usize) -> usize {
        let bitmap = if self.has_null {
            self.columns.div_ceil(8)
        } else {
            0
        };
        maxalign(header + bitmap) + self.data
    }
}

/// `HeapTupleHeaderData` before its null bitmap: the length of a composite
/// datum (`heap_form_tuple`, then `HeapTupleHeaderSetDatumLength`).
const COMPOSITE_HEADER: usize = 23;

/// The length of the composite datum of `fields`: a field is `None` when it
/// has no PostgreSQL size here and `Some(None)` for NULL.
fn composite_length(fields: impl Iterator<Item = Option<Option<Stored>>>) -> Option<usize> {
    let mut tuple = TupleData::default();
    for field in fields {
        tuple.push(field?);
    }
    Some(tuple.length(COMPOSITE_HEADER))
}

/// The stored form of the value at byte `at` of an encoded row, whose tag is
/// `tag`: `None` when it has no PostgreSQL size here, `Some(None)` for NULL.
fn stored_projected(
    sizing: Option<&Sizing<'_>>,
    row: &[u8],
    tag: u8,
    at: usize,
    hint: Option<i32>,
) -> Option<Option<Stored>> {
    if matches!(tag, 19 | 51 | 202 | 204) {
        let mut fields = [None::<Stored>; crate::sql::exec::MAX_PROJ];
        let mut count = 0usize;
        crate::sql::exec::try_each_record_field(row, tag, at, &mut |type_oid, tag, at| {
            *fields.get_mut(count)? = stored_projected(sizing, row, tag, at, Some(type_oid))?;
            count += 1;
            Some(())
        })?;
        let length = composite_length(fields[..count].iter().map(|&field| Some(field)))?;
        return Some(Some(Stored {
            layout: crate::sql::type_layouts::builtin(2249)?,
            payload: length - 4,
        }));
    }
    let (value, _) = crate::sql::exec::decode_projected_value(row, tag, at);
    if value.is_null() {
        return Some(None);
    }
    stored(sizing, hint, &value).map(Some)
}

/// The stored form of column `column` of an encoded row.
pub(crate) fn stored_column(
    sizing: Option<&Sizing<'_>>,
    row: &[u8],
    column: usize,
) -> Option<Option<Stored>> {
    let mut at = 2usize;
    for _ in 0..column {
        let tag = row[at];
        at += 1;
        at += crate::sql::exec::decode_projected_value(row, tag, at).1;
    }
    let tag = row[at];
    stored_projected(sizing, row, tag, at + 1, None)
}

/// The `t_len` of the minimal tuple (`heap_form_minimal_tuple`) of `values`,
/// each sized by its own type, followed by `tids` row identities (`ctid`
/// junk columns a row lock adds); `None` when a value has no PostgreSQL size
/// here.
pub(crate) fn minimal_tuple_len_with_tids<'a>(
    sizing: Option<&Sizing<'_>>,
    values: impl Iterator<Item = Datum<'a>>,
    tids: usize,
) -> Option<usize> {
    minimal_tuple_len(
        values.map(|value| {
            if value.is_null() {
                Some(None)
            } else {
                stored(sizing, None, &value).map(Some)
            }
        }),
        tids,
    )
}

/// [`minimal_tuple_len_with_tids`] of columns already sized.
fn minimal_tuple_len(
    columns: impl Iterator<Item = Option<Option<Stored>>>,
    tids: usize,
) -> Option<usize> {
    let mut tuple = TupleData::default();
    for column in columns {
        tuple.push(column?);
    }
    for _ in 0..tids {
        tuple.columns += 1;
        tuple.data = align_to(tuple.data, b's') + 6;
    }
    Some(tuple.length(MINIMAL_TUPLE_HEADER))
}

/// The `t_len` of the minimal tuple of a projected row's values at
/// `columns` (every value when `None`).
pub(crate) fn projected_tuple_len(
    sizing: Option<&Sizing<'_>>,
    row: &[u8],
    columns: Option<&[usize]>,
) -> Option<usize> {
    projected_tuple_len_with_tids(sizing, row, columns, 0)
}

/// [`projected_tuple_len`] with `tids` row identities after the values.
pub(crate) fn projected_tuple_len_with_tids(
    sizing: Option<&Sizing<'_>>,
    row: &[u8],
    columns: Option<&[usize]>,
    tids: usize,
) -> Option<usize> {
    if let Some(columns) = columns {
        return minimal_tuple_len(
            columns
                .iter()
                .map(|&column| stored_column(sizing, row, column)),
            tids,
        );
    }
    let width = crate::sql::exec::projected_row_width(row);
    let mut at = 2usize;
    minimal_tuple_len(
        (0..width).map(|_| {
            let tag = row[at];
            at += 1;
            let size = crate::sql::exec::decode_projected_value(row, tag, at).1;
            let column = stored_projected(sizing, row, tag, at, None);
            at += size;
            column
        }),
        tids,
    )
}

/// `datumGetSize` of the value a datum sort copies: nothing for a null,
/// a pass-by-value datum's eight bytes, a fixed-width value's length, or a
/// varlena: with a one-byte header when its data is short enough and it was
/// read from a stored column (`computed` false), as tuple forming packs it.
pub(crate) fn datum_written_size(value: Option<Stored>, computed: bool) -> usize {
    let Some(Stored { layout, payload }) = value else {
        return 0;
    };
    if layout.by_value {
        return core::mem::size_of::<usize>();
    }
    if layout.length > 0 {
        return layout.length as usize;
    }
    // A stored column holds a short value with a one-byte header; a value an
    // expression computed has the four-byte one.
    if !computed && layout.storage != b'p' && payload < SHORT_VARLENA_MAX {
        payload + 1
    } else {
        payload + 4
    }
}

/// [`datum_written_size`] of `value`; `None` when it has no PostgreSQL size
/// here.
pub(crate) fn value_written_size(
    sizing: Option<&Sizing<'_>>,
    value: &Datum<'_>,
    computed: bool,
) -> Option<usize> {
    let stored = if value.is_null() {
        None
    } else {
        Some(stored(sizing, None, value)?)
    };
    Some(datum_written_size(stored, computed))
}

/// [`datum_written_size`] of the value at `column` of a projected row (the
/// first when `None`); `None` when it has no PostgreSQL size here.
pub(crate) fn sorted_datum_size(
    sizing: Option<&Sizing<'_>>,
    row: &[u8],
    column: Option<usize>,
    computed: bool,
) -> Option<usize> {
    Some(datum_written_size(
        stored_column(sizing, row, column.unwrap_or(0))?,
        computed,
    ))
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
pub(crate) const MAX_MERGE_ORDER: usize = 500;

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
    /// The array of a sort reset after a batch that left it at its initial
    /// size is not charged again (`tuplesort_begin_batch`).
    reused_array: bool,
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
            reused_array: false,
            run_bytes: 0,
            tapes: None,
        }
    }

    fn array_space(slots: usize) -> i64 {
        (maxalign(slots * SORT_TUPLE) + CHUNK_HEADER) as i64
    }

    /// This sort began on the array the previous batch of its tuplesort kept
    /// (`tuplesort_reset`), which `availMem` does not charge.
    pub(crate) fn reuse_array(mut self) -> Self {
        self.available += Self::array_space(INITIAL_SORT_SLOTS);
        self.reused_array = true;
        self
    }

    /// Whether the sort left its array at a size a reset frees and
    /// allocates anew (charging it again).
    pub(crate) fn array_regrown(&self) -> bool {
        self.tapes.is_some() || self.slots != INITIAL_SORT_SLOTS
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
    ///
    /// Returns the row count of the run this row's arrival made
    /// `dumptuples` write, if it did.
    pub(crate) fn push(&mut self, length: usize, written: usize) -> Option<usize> {
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
                return Some(self.dump());
            }
            return None;
        }
        if self.heap {
            return None;
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
            return None;
        }
        if self.count >= self.slots || self.available < 0 {
            self.begin_tapes();
            return Some(self.dump());
        }
        None
    }

    /// Whether the rows pushed so far outgrew `work_mem`, so the sort writes
    /// its runs to tapes (`inittapes` ran).
    pub(crate) fn spilled(&self) -> bool {
        self.tapes.is_some()
    }

    /// The tapes a spilled sort merges (`tuplesort_merge_order`).
    pub(crate) fn merge_order(&self) -> Option<usize> {
        self.tapes.as_ref().map(|tapes| tapes.max_tapes)
    }

    /// `tuplesort_performsort`'s last `dumptuples`: the rows still in memory
    /// become the final run. Returns its row count.
    pub(crate) fn finish_run(&mut self) -> Option<usize> {
        (self.tapes.is_some() && self.count > 0).then(|| self.dump())
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
    fn dump(&mut self) -> usize {
        let tapes = self.tapes.as_mut().expect("tapes begun");
        tapes.tape_bytes[tapes.runs % tapes.max_tapes] += self.run_bytes + 4;
        tapes.runs += 1;
        let rows = self.count;
        self.run_bytes = 0;
        self.count = 0;
        self.available += self.tuple_memory;
        self.tuple_memory = 0;
        rows
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
        let array = Self::array_space(self.slots)
            - if self.reused_array {
                Self::array_space(INITIAL_SORT_SLOTS)
            } else {
                0
            };
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
#[cfg(test)]
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
    /// The rows went to a file at some point (`usedDisk`).
    used_disk: bool,
    /// Rows are written with a trailing length word (`EXEC_FLAG_BACKWARD`).
    backward: bool,
    /// What the rows in memory write when the store spills.
    resident_written: i64,
}

impl Tuplestore {
    /// A store whose readers read backwards, so its file holds a trailing
    /// length word after each row.
    pub(crate) fn backward(mut self) -> Self {
        self.backward = true;
        self
    }

    /// `writetup_heap`: the length word, the tuple after its
    /// `MINIMAL_TUPLE_DATA_OFFSET` bytes and a trailing length word of a
    /// backward store.
    const fn written(&self, length: usize) -> i64 {
        (length - 10 + 4 + if self.backward { 4 } else { 0 }) as i64
    }

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
            used_disk: false,
            backward: false,
            resident_written: 0,
        }
    }

    const fn array_space(slots: usize) -> i64 {
        (maxalign(slots * 8) + CHUNK_HEADER) as i64
    }

    /// The rows `memtupcount` holds, deleted ones included.
    pub(crate) fn count(&self) -> usize {
        self.count
    }

    /// Whether the rows overflowed `work_mem` into a temporary file.
    pub(crate) fn spilled(&self) -> bool {
        self.file.is_some()
    }

    /// `tuplestore_trim` freeing a row whose minimal tuple is `length`
    /// bytes.
    pub(crate) fn release(&mut self, length: usize) {
        self.available += generation_chunk_space(length) as i64;
        self.resident_written -= self.written(length);
    }

    /// `tuplestore_trim` sliding the pointer array down `removed` rows.
    pub(crate) fn slide(&mut self, removed: usize) {
        self.count -= removed;
    }

    /// `tuplestore_updatemax`.
    pub(crate) fn update_most(&mut self) {
        let used = match self.file {
            Some(bytes) => {
                self.used_disk = true;
                bytes
            }
            None => self.allowed - self.available,
        };
        self.most = self.most.max(used);
    }

    /// Stores a row whose minimal tuple is `length` bytes
    /// (`tuplestore_puttupleslot`).
    pub(crate) fn put(&mut self, length: usize) {
        let written = self.written(length);
        if let Some(bytes) = &mut self.file {
            *bytes += written;
            return;
        }
        self.available -= generation_chunk_space(length) as i64;
        self.resident_written += written;
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
        // `dumptuples` writes the rows in memory to the file.
        self.update_most();
        self.file = Some(self.resident_written);
        self.count = 0;
    }

    /// Drops every row and keeps the array (`tuplestore_clear`).
    pub(crate) fn clear(&mut self) {
        self.update_most();
        self.file = None;
        self.available = self.allowed - Self::array_space(self.slots);
        self.count = 0;
        self.resident_written = 0;
    }

    /// The figures `EXPLAIN` prints: whether the rows went to disk and the
    /// most bytes used.
    pub(crate) fn finish(mut self) -> (bool, usize) {
        self.update_most();
        (self.used_disk, self.most as usize)
    }
}

/// `sizeof(TupleHashEntryData)`.
const TUPLE_HASH_ENTRY: usize = 16;
/// `sizeof(AggStatePerGroupData)`.
const PER_GROUP_STATE: usize = 16;
/// The most by-reference transition states a modelled hash aggregate keeps.
pub(crate) const MAX_SIZED_STATES: usize = 8;

/// What sizes a hash aggregate's memory, decided when it is planned.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) struct HashAggSizing {
    /// `hash_agg_set_limits`: the memory and groups a table holds before
    /// the aggregate spills (`hash_agg_check_limits`).
    pub(crate) memory_limit: f64,
    pub(crate) groups_limit: u64,
    /// The buckets `build_hash_table` asks for.
    pub(crate) buckets: usize,
    /// The planner's estimate of a group's bytes (`hash_agg_entry_size`) and
    /// of the groups, which size the partitions of a first spill.
    pub(crate) entry_size: f64,
    pub(crate) groups: f64,
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
    /// A new group put the table past its limits, so PostgreSQL's aggregate
    /// spills the groups after it to partitions.
    spilled: bool,
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
            spilled: false,
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
        // `hash_agg_check_limits` after each new group.
        if self.peak() as f64 > self.sizing.memory_limit
            || self.entries as u64 > self.sizing.groups_limit
        {
            self.spilled = true;
        }
    }

    /// Brings the bucket array up to `size` buckets after the table grew
    /// for a run that grew too long, which no fill factor announced.
    pub(crate) fn grow_buckets_to(&mut self, size: usize) {
        while self.buckets < size {
            self.allocate_buckets(self.buckets * 2);
        }
    }

    /// `agg_refill_hash_table` starting a batch: the entries and transition
    /// values contexts are reset to their first blocks, the bucket array
    /// stays, and `hash_agg_set_limits` gives new limits.
    pub(crate) fn start_batch(&mut self, memory_limit: f64, groups_limit: u64) {
        self.entries = 0;
        self.entry_blocks = DEFAULT_INIT_BLOCK;
        self.entry_free = DEFAULT_INIT_BLOCK - BUMP_CONTEXT - BUMP_BLOCK_HEADER;
        self.entry_next_block = DEFAULT_INIT_BLOCK;
        self.transition = AllocSetBlocks::new(work_mem_block());
        self.sizing.memory_limit = memory_limit;
        self.sizing.groups_limit = groups_limit;
        self.spilled = false;
    }

    /// The groups in the table.
    pub(crate) fn entries(&self) -> usize {
        self.entries
    }

    /// The bytes the by-reference transition values' context holds
    /// (`hashkey_mem` of `hash_agg_update_metrics`).
    pub(crate) fn transition_allocated(&self) -> usize {
        self.transition.allocated
    }

    /// Whether the aggregate spilled (`hash_agg_enter_spill_mode`).
    pub(crate) fn spilled(&self) -> bool {
        self.spilled
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
