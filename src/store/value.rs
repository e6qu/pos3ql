//! Object-resident secondary-index generations.
//!
//! A generation is an immutable sequence of key-only data blocks plus one
//! roster block naming them. Each entry carries the equality hash, encoded
//! key tuple, row identity, and commit LSN. Readers can therefore reject an
//! equality miss without fetching a row; every candidate is still checked
//! against the authoritative MVCC row image by the storage layer.

use super::{BlockId, BlockStore, BlockType, MAX_PAYLOAD, StoreError};

const ENTRY_HEADER: usize = 8 + 8 + 8 + 4;
// Checkpoint construction externally sorts the complete entry with an
// eight-byte stable ordinal inside an SST row, whose own header is 20 bytes.
pub(crate) const VALUE_INDEX_KEY_MAX: usize = MAX_PAYLOAD - ENTRY_HEADER - 8 - 20;
const ROSTER_CHAINED: u32 = 1 << 31;
const ROSTER_BLOCK_FILTERS: u32 = 1 << 30;
const ROSTER_ORDERED_BOUNDS: u32 = 1 << 29;
const ROSTER_COUNT_MASK: u32 = !(ROSTER_CHAINED | ROSTER_BLOCK_FILTERS | ROSTER_ORDERED_BOUNDS);
const ROSTER_HEADER: usize = 4 + 32;
/// Enough bits for the largest possible data block to retain a useful false
/// positive rate while still grouping dozens of data blocks under one roster.
const BLOCK_FILTER_BYTES: usize = 8 * 1024;
const FILTERED_BLOCK_REF: usize = 32 + BLOCK_FILTER_BYTES;
const ORDERED_BLOCK_REF_HEADER: usize = FILTERED_BLOCK_REF + 4 + 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ValueIndexPosition {
    Before,
    Match,
    After,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ValueIndexHandle {
    pub(crate) roster: BlockId,
    pub(crate) entries: u64,
    /// Manifest LSN whose committed table image this generation covers.
    pub(crate) published_lsn: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ValueIndexError {
    KeyTooLarge,
    KeyOutOfOrder,
    Corrupt,
    Store(StoreError),
}

impl From<StoreError> for ValueIndexError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

/// Allocation-owning checkpoint writer, constructed before memory freezes.
pub(crate) struct ValueIndexWriter {
    pending: Box<[u8]>,
    pending_len: usize,
    pending_filter: Box<[u8]>,
    pending_first: Option<(usize, usize)>,
    pending_last: Option<(usize, usize)>,
    roster: Box<[u8]>,
    roster_len: usize,
    block_count: usize,
    roster_tail: Option<BlockId>,
    entries: u64,
}

impl ValueIndexWriter {
    pub(crate) fn new() -> Self {
        Self {
            pending: vec![0; MAX_PAYLOAD].into_boxed_slice(),
            pending_len: 0,
            pending_filter: vec![0; BLOCK_FILTER_BYTES].into_boxed_slice(),
            pending_first: None,
            pending_last: None,
            roster: vec![0; MAX_PAYLOAD].into_boxed_slice(),
            roster_len: ROSTER_HEADER,
            block_count: 0,
            roster_tail: None,
            entries: 0,
        }
    }

    pub(crate) fn budget_bytes() -> usize {
        MAX_PAYLOAD + BLOCK_FILTER_BYTES + MAX_PAYLOAD
    }

    pub(crate) fn reset(&mut self) {
        self.pending_len = 0;
        self.pending_filter.fill(0);
        self.pending_first = None;
        self.pending_last = None;
        self.roster_len = ROSTER_HEADER;
        self.block_count = 0;
        self.roster_tail = None;
        self.entries = 0;
    }

    pub(crate) fn append(
        &mut self,
        store: &mut dyn BlockStore,
        hash: u64,
        rowid: u64,
        commit_lsn: u64,
        key: &[u8],
        compare: &mut impl FnMut(&[u8], &[u8]) -> core::cmp::Ordering,
    ) -> Result<(), ValueIndexError> {
        if key.len() > VALUE_INDEX_KEY_MAX {
            return Err(ValueIndexError::KeyTooLarge);
        }
        if self.entries > 0 {
            // A block is flushed only after its final key has been compared
            // with the first key of its successor, so the pending buffer is
            // also the ordering cursor and needs no duplicate maximum-key
            // allocation.
            let (at, len) = self
                .pending_last
                .expect("a non-empty writer retains its pending last key");
            if compare(&self.pending[at..at + len], key).is_gt() {
                return Err(ValueIndexError::KeyOutOfOrder);
            }
        }
        let bytes = ENTRY_HEADER + key.len();
        if self.pending_len + bytes > MAX_PAYLOAD {
            self.flush(store)?;
        }
        super::bloom::insert(&mut self.pending_filter, hash);
        let at = self.pending_len;
        self.pending[at..at + 8].copy_from_slice(&hash.to_le_bytes());
        self.pending[at + 8..at + 16].copy_from_slice(&rowid.to_le_bytes());
        self.pending[at + 16..at + 24].copy_from_slice(&commit_lsn.to_le_bytes());
        self.pending[at + 24..at + 28].copy_from_slice(&(key.len() as u32).to_le_bytes());
        self.pending[at + ENTRY_HEADER..at + bytes].copy_from_slice(key);
        let key_extent = (at + ENTRY_HEADER, key.len());
        self.pending_first.get_or_insert(key_extent);
        self.pending_last = Some(key_extent);
        self.pending_len += bytes;
        self.entries += 1;
        Ok(())
    }

    fn flush(&mut self, store: &mut dyn BlockStore) -> Result<(), ValueIndexError> {
        if self.pending_len == 0 {
            return Ok(());
        }
        let id = store.put(
            &self.pending[..self.pending_len],
            BlockType::ValueIndexData,
            0,
        )?;
        let (first_at, first_len) = self.pending_first.expect("non-empty block has first key");
        let (last_at, last_len) = self.pending_last.expect("non-empty block has last key");
        let bounded_bytes = ORDERED_BLOCK_REF_HEADER + first_len + last_len;
        let ref_bytes = if bounded_bytes <= MAX_PAYLOAD - ROSTER_HEADER {
            bounded_bytes
        } else {
            // An exceptionally large key still remains indexable. Its block
            // cannot carry inline boundary copies, so range scans conservatively
            // read that block instead of turning a format limit into data loss.
            ORDERED_BLOCK_REF_HEADER
        };
        if self.roster_len + ref_bytes > MAX_PAYLOAD {
            self.flush_roster(store)?;
        }
        let at = self.roster_len;
        self.roster[at..at + 32].copy_from_slice(&id.0);
        self.roster[at + 32..at + FILTERED_BLOCK_REF].copy_from_slice(&self.pending_filter);
        let bounds_at = at + FILTERED_BLOCK_REF;
        let inline_bounds = ref_bytes != ORDERED_BLOCK_REF_HEADER;
        self.roster[bounds_at..bounds_at + 4]
            .copy_from_slice(&(if inline_bounds { first_len as u32 } else { 0 }).to_le_bytes());
        self.roster[bounds_at + 4..bounds_at + 8]
            .copy_from_slice(&(if inline_bounds { last_len as u32 } else { 0 }).to_le_bytes());
        if inline_bounds {
            let keys_at = bounds_at + 8;
            self.roster[keys_at..keys_at + first_len]
                .copy_from_slice(&self.pending[first_at..first_at + first_len]);
            self.roster[keys_at + first_len..keys_at + first_len + last_len]
                .copy_from_slice(&self.pending[last_at..last_at + last_len]);
        }
        self.roster_len += ref_bytes;
        self.block_count += 1;
        self.pending_len = 0;
        self.pending_filter.fill(0);
        self.pending_first = None;
        self.pending_last = None;
        Ok(())
    }

    /// Publishes one immutable roster node pointing at the previously written
    /// node. Only one node's identities stay resident, so generation size is
    /// not bounded by a single roster block.
    fn flush_roster(&mut self, store: &mut dyn BlockStore) -> Result<(), ValueIndexError> {
        let count = u32::try_from(self.block_count).map_err(|_| ValueIndexError::Corrupt)?;
        self.roster[..4].copy_from_slice(
            &(count | ROSTER_CHAINED | ROSTER_BLOCK_FILTERS | ROSTER_ORDERED_BOUNDS).to_le_bytes(),
        );
        self.roster[4..36].fill(0);
        if let Some(previous) = self.roster_tail {
            self.roster[4..36].copy_from_slice(&previous.0);
        }
        // The roster root is written with a stable lsn (0): a content-addressed
        // block must re-PUT the same bytes for the same payload, but the header
        // lsn is the checkpoint's and varies across incarnations — so it would
        // break write-idempotency for an identical rebuilt index. The lsn is
        // vestigial in the block (never read back); `published_lsn` rides the
        // handle into the manifest instead.
        self.roster_tail = Some(store.put(
            &self.roster[..self.roster_len],
            BlockType::ValueIndexRoster,
            0,
        )?);
        self.block_count = 0;
        self.roster_len = ROSTER_HEADER;
        Ok(())
    }

    pub(crate) fn finish(
        &mut self,
        store: &mut dyn BlockStore,
        published_lsn: u64,
    ) -> Result<Option<ValueIndexHandle>, ValueIndexError> {
        self.flush(store)?;
        self.flush_roster(store)?;
        let roster = self.roster_tail.expect("finish writes a roster root");
        Ok(Some(ValueIndexHandle {
            roster,
            entries: self.entries,
            published_lsn,
        }))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RosterFormat {
    Plain,
    Filtered,
    Ordered,
}

fn roster_layout(
    raw_count: u32,
    roster_len: usize,
) -> Result<(usize, RosterFormat), ValueIndexError> {
    if raw_count & ROSTER_CHAINED == 0 {
        return Err(ValueIndexError::Corrupt);
    }
    let block_count = (raw_count & ROSTER_COUNT_MASK) as usize;
    let format = if raw_count & ROSTER_ORDERED_BOUNDS != 0 {
        if raw_count & ROSTER_BLOCK_FILTERS == 0 {
            return Err(ValueIndexError::Corrupt);
        }
        RosterFormat::Ordered
    } else if raw_count & ROSTER_BLOCK_FILTERS != 0 {
        RosterFormat::Filtered
    } else {
        RosterFormat::Plain
    };
    if format != RosterFormat::Ordered {
        let stride = if format == RosterFormat::Filtered {
            FILTERED_BLOCK_REF
        } else {
            32
        };
        let expected_len = block_count
            .checked_mul(stride)
            .and_then(|bytes| ROSTER_HEADER.checked_add(bytes))
            .ok_or(ValueIndexError::Corrupt)?;
        if roster_len != expected_len {
            return Err(ValueIndexError::Corrupt);
        }
    }
    Ok((block_count, format))
}

type RosterRef<'a> = (BlockId, Option<&'a [u8]>, Option<(&'a [u8], &'a [u8])>);

fn roster_ref<'a>(
    bytes: &'a [u8],
    format: RosterFormat,
    cursor: &mut usize,
) -> Result<RosterRef<'a>, ValueIndexError> {
    let fixed = match format {
        RosterFormat::Plain => 32,
        RosterFormat::Filtered => FILTERED_BLOCK_REF,
        RosterFormat::Ordered => ORDERED_BLOCK_REF_HEADER,
    };
    let fixed_end = cursor.checked_add(fixed).ok_or(ValueIndexError::Corrupt)?;
    if fixed_end > bytes.len() {
        return Err(ValueIndexError::Corrupt);
    }
    let mut id = [0; 32];
    id.copy_from_slice(&bytes[*cursor..*cursor + 32]);
    let filter = match format {
        RosterFormat::Plain => None,
        RosterFormat::Filtered | RosterFormat::Ordered => {
            Some(&bytes[*cursor + 32..*cursor + FILTERED_BLOCK_REF])
        }
    };
    let bounds = if format == RosterFormat::Ordered {
        let header = *cursor + FILTERED_BLOCK_REF;
        let first_len = u32::from_le_bytes(bytes[header..header + 4].try_into().unwrap()) as usize;
        let last_len =
            u32::from_le_bytes(bytes[header + 4..header + 8].try_into().unwrap()) as usize;
        if (first_len == 0) != (last_len == 0) {
            return Err(ValueIndexError::Corrupt);
        }
        let end = fixed_end
            .checked_add(first_len)
            .and_then(|at| at.checked_add(last_len))
            .filter(|end| *end <= bytes.len())
            .ok_or(ValueIndexError::Corrupt)?;
        let result = (first_len != 0).then_some((
            &bytes[fixed_end..fixed_end + first_len],
            &bytes[fixed_end + first_len..end],
        ));
        *cursor = end;
        result
    } else {
        *cursor = fixed_end;
        None
    };
    Ok((BlockId(id), filter, bounds))
}

/// Walks every roster node and data-block identity in a generation. Returning
/// false from `visit` stops before any caller-owned keep-set can overflow.
pub(crate) fn walk_value_roster(
    store: &mut dyn BlockStore,
    root: BlockId,
    scratch: &mut [u8],
    mut visit: impl FnMut(BlockId) -> bool,
) -> Result<bool, ValueIndexError> {
    let mut next = Some(root);
    while let Some(roster) = next {
        if !visit(roster) {
            return Ok(false);
        }
        let (roster_len, kind) = store.get(&roster, scratch)?;
        if kind != BlockType::ValueIndexRoster || roster_len < 4 {
            return Err(ValueIndexError::Corrupt);
        }
        let raw_count = u32::from_le_bytes(scratch[..4].try_into().unwrap());
        let (block_count, format) = roster_layout(raw_count, roster_len)?;
        next = if scratch[4..36].iter().any(|byte| *byte != 0) {
            let mut id = [0; 32];
            id.copy_from_slice(&scratch[4..36]);
            Some(BlockId(id))
        } else {
            None
        };
        let mut cursor = ROSTER_HEADER;
        for _ in 0..block_count {
            let (id, _, _) = roster_ref(&scratch[..roster_len], format, &mut cursor)?;
            if !visit(id) {
                return Ok(false);
            }
        }
        if cursor != roster_len {
            return Err(ValueIndexError::Corrupt);
        }
    }
    Ok(true)
}

pub(crate) struct ValueIndexReader<'a> {
    roster: &'a mut [u8],
    data: &'a mut [u8],
}

impl<'a> ValueIndexReader<'a> {
    pub(crate) fn over(roster: &'a mut [u8], data: &'a mut [u8]) -> Self {
        Self { roster, data }
    }

    /// Visits every entry with `hash`. Runs may contain stale row versions;
    /// callers must recheck the current MVCC image and key.
    pub(crate) fn probe(
        &mut self,
        store: &mut dyn BlockStore,
        handle: &ValueIndexHandle,
        hash: u64,
        mut visit: impl FnMut(u64, u64, &[u8]),
    ) -> Result<(), ValueIndexError> {
        self.walk_inner(store, handle, Some(hash), |_, rowid, lsn, key| {
            visit(rowid, lsn, key);
        })
    }

    pub(crate) fn walk(
        &mut self,
        store: &mut dyn BlockStore,
        handle: &ValueIndexHandle,
        visit: impl FnMut(u64, u64, u64, &[u8]),
    ) -> Result<(), ValueIndexError> {
        self.walk_inner(store, handle, None, visit)
    }

    /// Visits only the ordered interval selected by `classify`. Legacy
    /// generations have no block bounds and remain readable through the same
    /// path, but cannot skip their data blocks until the next checkpoint.
    pub(crate) fn range(
        &mut self,
        store: &mut dyn BlockStore,
        handle: &ValueIndexHandle,
        mut classify: impl FnMut(&[u8]) -> ValueIndexPosition,
        mut visit: impl FnMut(u64, u64, u64, &[u8]),
    ) -> Result<(), ValueIndexError> {
        let mut next = Some(handle.roster);
        let mut roster_count = 0u64;
        while let Some(roster) = next {
            roster_count += 1;
            if roster_count > handle.entries.saturating_add(1) {
                return Err(ValueIndexError::Corrupt);
            }
            let (roster_len, kind) = store.get(&roster, self.roster)?;
            if kind != BlockType::ValueIndexRoster || roster_len < ROSTER_HEADER {
                return Err(ValueIndexError::Corrupt);
            }
            let raw_count = u32::from_le_bytes(self.roster[..4].try_into().unwrap());
            let (block_count, format) = roster_layout(raw_count, roster_len)?;
            next = roster_predecessor(&self.roster[..roster_len]);
            let mut cursor = ROSTER_HEADER;
            for _ in 0..block_count {
                let (id, _, bounds) = roster_ref(&self.roster[..roster_len], format, &mut cursor)?;
                if bounds.is_some_and(|(first, last)| {
                    classify(last) == ValueIndexPosition::Before
                        || classify(first) == ValueIndexPosition::After
                }) {
                    continue;
                }
                let (data_len, kind) = store.get(&id, self.data)?;
                if kind != BlockType::ValueIndexData {
                    return Err(ValueIndexError::Corrupt);
                }
                walk_data(&self.data[..data_len], |hash, rowid, lsn, key| {
                    if classify(key) == ValueIndexPosition::Match {
                        visit(hash, rowid, lsn, key);
                    }
                })?;
            }
            if cursor != roster_len {
                return Err(ValueIndexError::Corrupt);
            }
        }
        Ok(())
    }

    fn walk_inner(
        &mut self,
        store: &mut dyn BlockStore,
        handle: &ValueIndexHandle,
        target_hash: Option<u64>,
        mut visit: impl FnMut(u64, u64, u64, &[u8]),
    ) -> Result<(), ValueIndexError> {
        let mut seen = 0u64;
        let mut next = Some(handle.roster);
        let mut roster_count = 0u64;
        while let Some(roster) = next {
            roster_count += 1;
            if roster_count > handle.entries.saturating_add(1) {
                return Err(ValueIndexError::Corrupt);
            }
            let (roster_len, kind) = store.get(&roster, self.roster)?;
            if kind != BlockType::ValueIndexRoster || roster_len < 4 {
                return Err(ValueIndexError::Corrupt);
            }
            let raw_count = u32::from_le_bytes(self.roster[..4].try_into().unwrap());
            let (block_count, format) = roster_layout(raw_count, roster_len)?;
            next = roster_predecessor(&self.roster[..roster_len]);
            let mut roster_cursor = ROSTER_HEADER;
            for _ in 0..block_count {
                let (id, filter, _) =
                    roster_ref(&self.roster[..roster_len], format, &mut roster_cursor)?;
                if let Some(hash) = target_hash
                    && filter.is_some_and(|filter| !super::bloom::maybe_contains(filter, hash))
                {
                    continue;
                }
                let (data_len, kind) = store.get(&id, self.data)?;
                if kind != BlockType::ValueIndexData {
                    return Err(ValueIndexError::Corrupt);
                }
                walk_data(&self.data[..data_len], |hash, rowid, lsn, key| {
                    if target_hash.is_none_or(|target| target == hash) {
                        visit(hash, rowid, lsn, key);
                    }
                    seen += 1;
                })?;
            }
            if roster_cursor != roster_len {
                return Err(ValueIndexError::Corrupt);
            }
        }
        if target_hash.is_none() && seen != handle.entries {
            return Err(ValueIndexError::Corrupt);
        }
        Ok(())
    }
}

fn roster_predecessor(bytes: &[u8]) -> Option<BlockId> {
    bytes[4..36].iter().any(|byte| *byte != 0).then(|| {
        let mut id = [0; 32];
        id.copy_from_slice(&bytes[4..36]);
        BlockId(id)
    })
}

fn walk_data(
    data: &[u8],
    mut visit: impl FnMut(u64, u64, u64, &[u8]),
) -> Result<(), ValueIndexError> {
    let mut cursor = 0usize;
    while cursor < data.len() {
        if data.len() - cursor < ENTRY_HEADER {
            return Err(ValueIndexError::Corrupt);
        }
        let hash = u64::from_le_bytes(data[cursor..cursor + 8].try_into().unwrap());
        let rowid = u64::from_le_bytes(data[cursor + 8..cursor + 16].try_into().unwrap());
        let lsn = u64::from_le_bytes(data[cursor + 16..cursor + 24].try_into().unwrap());
        let key_len =
            u32::from_le_bytes(data[cursor + 24..cursor + 28].try_into().unwrap()) as usize;
        let end = cursor
            .checked_add(ENTRY_HEADER)
            .and_then(|start| start.checked_add(key_len))
            .filter(|end| *end <= data.len())
            .ok_or(ValueIndexError::Corrupt)?;
        visit(hash, rowid, lsn, &data[cursor + ENTRY_HEADER..end]);
        cursor = end;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem::budget::Budget;
    use crate::store::memory::MemoryBlockStore;

    #[test]
    fn generation_round_trips_and_filters_hashes() {
        let mut budget = Budget::new(8 << 20);
        let mut store = MemoryBlockStore::new(&mut budget, "value test", 4 << 20, 32).unwrap();
        let mut writer = ValueIndexWriter::new();
        let mut compare = |left: &[u8], right: &[u8]| left.cmp(right);
        writer
            .append(&mut store, 7, 11, 3, b"alpha", &mut compare)
            .unwrap();
        writer
            .append(&mut store, 9, 12, 4, b"beta", &mut compare)
            .unwrap();
        let handle = writer.finish(&mut store, 10).unwrap().unwrap();
        let mut roster = vec![0; MAX_PAYLOAD];
        let mut data = vec![0; MAX_PAYLOAD];
        let mut reader = ValueIndexReader::over(&mut roster, &mut data);
        let mut found = Vec::new();
        reader
            .probe(&mut store, &handle, 9, |rowid, lsn, key| {
                found.push((rowid, lsn, key.to_vec()));
            })
            .unwrap();
        assert_eq!(found, [(12, 4, b"beta".to_vec())]);
    }

    #[test]
    fn per_block_filters_skip_irrelevant_value_data() {
        let mut budget = Budget::new(8 << 20);
        let mut store = MemoryBlockStore::new(&mut budget, "value filters", 4 << 20, 32).unwrap();
        let mut writer = ValueIndexWriter::new();
        let first_key = vec![b'a'; MAX_PAYLOAD / 2];
        let second_key = vec![b'b'; MAX_PAYLOAD / 2];
        let mut compare = |left: &[u8], right: &[u8]| left.cmp(right);
        writer
            .append(&mut store, 7, 11, 3, &first_key, &mut compare)
            .unwrap();
        writer
            .append(&mut store, 9, 12, 4, &second_key, &mut compare)
            .unwrap();
        let handle = writer.finish(&mut store, 10).unwrap().unwrap();

        let mut roster = vec![0; MAX_PAYLOAD];
        let mut data = vec![0; MAX_PAYLOAD];
        let before_missing = store.reads();
        ValueIndexReader::over(&mut roster, &mut data)
            .probe(&mut store, &handle, 99, |_, _, _| {
                panic!("absent hash matched")
            })
            .unwrap();
        assert_eq!(
            store.reads() - before_missing,
            1,
            "an absent hash reads only the roster"
        );

        let before_present = store.reads();
        let mut found = 0;
        ValueIndexReader::over(&mut roster, &mut data)
            .probe(&mut store, &handle, 7, |rowid, _, _| {
                assert_eq!(rowid, 11);
                found += 1;
            })
            .unwrap();
        assert_eq!(found, 1);
        assert_eq!(
            store.reads() - before_present,
            2,
            "a present hash reads the roster and only its candidate data block"
        );

        let before_walk = store.reads();
        ValueIndexReader::over(&mut roster, &mut data)
            .walk(&mut store, &handle, |_, _, _, _| {})
            .unwrap();
        assert_eq!(
            store.reads() - before_walk,
            3,
            "a full walk reads both data blocks"
        );

        let before_unbounded = store.reads();
        let mut range_matches = 0;
        ValueIndexReader::over(&mut roster, &mut data)
            .range(
                &mut store,
                &handle,
                |_| ValueIndexPosition::After,
                |_, _, _, _| range_matches += 1,
            )
            .unwrap();
        assert_eq!(range_matches, 0);
        assert_eq!(
            store.reads() - before_unbounded,
            3,
            "keys too large for inline bounds conservatively read their blocks"
        );
    }

    #[test]
    fn ordered_bounds_skip_disjoint_range_blocks() {
        let mut budget = Budget::new(16 << 20);
        let mut store = MemoryBlockStore::new(&mut budget, "value ranges", 12 << 20, 128).unwrap();
        let mut writer = ValueIndexWriter::new();
        let mut compare = |left: &[u8], right: &[u8]| left.cmp(right);
        for value in b'a'..=b'u' {
            let key = vec![value; 32 * 1024];
            writer
                .append(
                    &mut store,
                    u64::from(value),
                    u64::from(value - b'a' + 1),
                    1,
                    &key,
                    &mut compare,
                )
                .unwrap();
        }
        let handle = writer.finish(&mut store, 1).unwrap().unwrap();
        let mut roster = vec![0; MAX_PAYLOAD];
        let mut data = vec![0; MAX_PAYLOAD];
        let before = store.reads();
        let mut found = Vec::new();
        ValueIndexReader::over(&mut roster, &mut data)
            .range(
                &mut store,
                &handle,
                |key| match key[0].cmp(&b'h') {
                    core::cmp::Ordering::Less => ValueIndexPosition::Before,
                    core::cmp::Ordering::Equal => ValueIndexPosition::Match,
                    core::cmp::Ordering::Greater => ValueIndexPosition::After,
                },
                |_, rowid, _, _| found.push(rowid),
            )
            .unwrap();
        assert_eq!(found, [8]);
        assert_eq!(
            store.reads() - before,
            2,
            "one roster and only the intersecting data block are read"
        );
    }

    #[test]
    fn writer_rejects_keys_out_of_sql_order() {
        let mut budget = Budget::new(8 << 20);
        let mut store = MemoryBlockStore::new(&mut budget, "value ordering", 4 << 20, 32).unwrap();
        let mut writer = ValueIndexWriter::new();
        let mut compare = |left: &[u8], right: &[u8]| left.cmp(right);
        writer
            .append(&mut store, 1, 1, 1, b"beta", &mut compare)
            .unwrap();
        assert_eq!(
            writer
                .append(&mut store, 2, 2, 1, b"alpha", &mut compare)
                .unwrap_err(),
            ValueIndexError::KeyOutOfOrder
        );
    }

    #[test]
    fn pre_filter_rosters_remain_readable() {
        let mut budget = Budget::new(8 << 20);
        let mut store =
            MemoryBlockStore::new(&mut budget, "old value format", 4 << 20, 32).unwrap();
        let key = b"old";
        let mut entry = vec![0u8; ENTRY_HEADER + key.len()];
        entry[..8].copy_from_slice(&17u64.to_le_bytes());
        entry[8..16].copy_from_slice(&23u64.to_le_bytes());
        entry[16..24].copy_from_slice(&29u64.to_le_bytes());
        entry[24..28].copy_from_slice(&(key.len() as u32).to_le_bytes());
        entry[ENTRY_HEADER..].copy_from_slice(key);
        let data_id = store.put(&entry, BlockType::ValueIndexData, 0).unwrap();
        let mut roster_bytes = [0u8; ROSTER_HEADER + 32];
        roster_bytes[..4].copy_from_slice(&(1 | ROSTER_CHAINED).to_le_bytes());
        roster_bytes[ROSTER_HEADER..].copy_from_slice(&data_id.0);
        let roster_id = store
            .put(&roster_bytes, BlockType::ValueIndexRoster, 0)
            .unwrap();
        let handle = ValueIndexHandle {
            roster: roster_id,
            entries: 1,
            published_lsn: 31,
        };
        let mut roster = vec![0; MAX_PAYLOAD];
        let mut data = vec![0; MAX_PAYLOAD];
        let mut found = None;
        ValueIndexReader::over(&mut roster, &mut data)
            .probe(&mut store, &handle, 17, |rowid, lsn, key| {
                found = Some((rowid, lsn, key.to_vec()));
            })
            .unwrap();
        assert_eq!(found, Some((23, 29, b"old".to_vec())));
    }

    #[test]
    fn filtered_pre_ordering_rosters_remain_readable() {
        let mut budget = Budget::new(8 << 20);
        let mut store =
            MemoryBlockStore::new(&mut budget, "filtered old value format", 4 << 20, 32).unwrap();
        let key = b"filtered";
        let mut entry = vec![0u8; ENTRY_HEADER + key.len()];
        entry[..8].copy_from_slice(&41u64.to_le_bytes());
        entry[8..16].copy_from_slice(&43u64.to_le_bytes());
        entry[16..24].copy_from_slice(&47u64.to_le_bytes());
        entry[24..28].copy_from_slice(&(key.len() as u32).to_le_bytes());
        entry[ENTRY_HEADER..].copy_from_slice(key);
        let data_id = store.put(&entry, BlockType::ValueIndexData, 0).unwrap();
        let mut roster_bytes = vec![0u8; ROSTER_HEADER + FILTERED_BLOCK_REF];
        roster_bytes[..4]
            .copy_from_slice(&(1 | ROSTER_CHAINED | ROSTER_BLOCK_FILTERS).to_le_bytes());
        roster_bytes[ROSTER_HEADER..ROSTER_HEADER + 32].copy_from_slice(&data_id.0);
        super::super::bloom::insert(
            &mut roster_bytes[ROSTER_HEADER + 32..ROSTER_HEADER + FILTERED_BLOCK_REF],
            41,
        );
        let roster_id = store
            .put(&roster_bytes, BlockType::ValueIndexRoster, 0)
            .unwrap();
        let handle = ValueIndexHandle {
            roster: roster_id,
            entries: 1,
            published_lsn: 47,
        };
        let mut roster = vec![0; MAX_PAYLOAD];
        let mut data = vec![0; MAX_PAYLOAD];
        let mut found = None;
        ValueIndexReader::over(&mut roster, &mut data)
            .probe(&mut store, &handle, 41, |rowid, lsn, key| {
                found = Some((rowid, lsn, key.to_vec()));
            })
            .unwrap();
        assert_eq!(found, Some((43, 47, b"filtered".to_vec())));
    }

    #[test]
    fn malformed_ordered_bounds_are_rejected() {
        let mut budget = Budget::new(8 << 20);
        let mut store =
            MemoryBlockStore::new(&mut budget, "bad value bounds", 4 << 20, 32).unwrap();
        let mut roster_bytes = vec![0u8; ROSTER_HEADER + ORDERED_BLOCK_REF_HEADER];
        roster_bytes[..4].copy_from_slice(
            &(1 | ROSTER_CHAINED | ROSTER_BLOCK_FILTERS | ROSTER_ORDERED_BOUNDS).to_le_bytes(),
        );
        let bounds = ROSTER_HEADER + FILTERED_BLOCK_REF;
        roster_bytes[bounds..bounds + 4].copy_from_slice(&1u32.to_le_bytes());
        let roster_id = store
            .put(&roster_bytes, BlockType::ValueIndexRoster, 0)
            .unwrap();
        let handle = ValueIndexHandle {
            roster: roster_id,
            entries: 1,
            published_lsn: 1,
        };
        let mut roster = vec![0; MAX_PAYLOAD];
        let mut data = vec![0; MAX_PAYLOAD];
        assert_eq!(
            ValueIndexReader::over(&mut roster, &mut data)
                .range(
                    &mut store,
                    &handle,
                    |_| ValueIndexPosition::Match,
                    |_, _, _, _| {},
                )
                .unwrap_err(),
            ValueIndexError::Corrupt
        );
    }

    #[test]
    fn chained_roster_roots_are_walked_without_a_flat_block_limit() {
        let mut budget = Budget::new(8 << 20);
        let mut store = MemoryBlockStore::new(&mut budget, "value chain", 4 << 20, 32).unwrap();
        let mut writer = ValueIndexWriter::new();
        writer
            .append(
                &mut store,
                7,
                11,
                3,
                b"alpha",
                &mut |left: &[u8], right: &[u8]| left.cmp(right),
            )
            .unwrap();
        let base = writer.finish(&mut store, 3).unwrap().unwrap();

        // A root with no local data blocks and a prior roster is the shape
        // produced when a generation ends exactly on a full roster node.
        let mut chained = [0u8; ROSTER_HEADER];
        chained[..4].copy_from_slice(&ROSTER_CHAINED.to_le_bytes());
        chained[4..36].copy_from_slice(&base.roster.0);
        let root = store.put(&chained, BlockType::ValueIndexRoster, 3).unwrap();
        let handle = ValueIndexHandle {
            roster: root,
            ..base
        };

        let mut roster = vec![0; MAX_PAYLOAD];
        let mut data = vec![0; MAX_PAYLOAD];
        let mut seen = 0;
        ValueIndexReader::over(&mut roster, &mut data)
            .probe(&mut store, &handle, 7, |rowid, _, key| {
                assert_eq!(rowid, 11);
                assert_eq!(key, b"alpha");
                seen += 1;
            })
            .unwrap();
        assert_eq!(seen, 1);

        let mut identities = 0;
        assert!(
            walk_value_roster(&mut store, root, &mut roster, |_| {
                identities += 1;
                true
            })
            .unwrap()
        );
        assert_eq!(identities, 3, "two roster roots plus one data block");
    }

    #[test]
    fn obsolete_unchained_rosters_are_rejected() {
        let mut budget = Budget::new(8 << 20);
        let mut store = MemoryBlockStore::new(&mut budget, "value format", 4 << 20, 32).unwrap();
        let obsolete = store
            .put(&0u32.to_le_bytes(), BlockType::ValueIndexRoster, 0)
            .unwrap();
        let handle = ValueIndexHandle {
            roster: obsolete,
            entries: 0,
            published_lsn: 1,
        };
        let mut roster = vec![0; MAX_PAYLOAD];
        let mut data = vec![0; MAX_PAYLOAD];
        assert_eq!(
            ValueIndexReader::over(&mut roster, &mut data)
                .walk(&mut store, &handle, |_, _, _, _| {})
                .unwrap_err(),
            ValueIndexError::Corrupt
        );
        assert_eq!(
            walk_value_roster(&mut store, obsolete, &mut roster, |_| true).unwrap_err(),
            ValueIndexError::Corrupt
        );
    }
}
