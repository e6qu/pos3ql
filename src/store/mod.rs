//! The block grid: the one unit every persisted byte travels in.
//!
//! A block is fixed-size, self-describing and checksummed, and it is
//! *content-addressed* — its identity is the SHA-256 of its payload, the way a
//! Loki chunk is keyed by its content. That single choice buys three things the
//! layers above would otherwise each have to arrange for themselves. Writing a
//! block twice is writing it once, so a retry after an ambiguous failure is
//! free rather than a duplicate. Nothing has to be overwritten, so only the
//! root of the tree needs compare-and-swap. And a block that reads back with
//! the wrong bytes cannot be mistaken for a different valid block, because its
//! name is what it should contain.
//!
//! The header carries a CRC-32C as well as the identity hash: the CRC catches a
//! damaged read cheaply on every access, while the hash is what makes the name
//! meaningful and is checked when a block arrives from somewhere untrusted.
//!
//! Nothing here allocates. Encoding writes into a caller-provided buffer and
//! decoding borrows from one, so a block lives in whatever pool its owner
//! reserved at startup.

macro_rules! delegate_async_block_reads {
    () => {
        fn enable_async_gets(&mut self) {
            self.inner.enable_async_gets();
        }

        fn disable_async_gets(&mut self) {
            self.inner.disable_async_gets();
        }

        fn async_gets_enabled(&self) -> bool {
            self.inner.async_gets_enabled()
        }

        fn async_read_slots(&self) -> usize {
            self.inner.async_read_slots()
        }

        fn async_reads_busy(&self) -> bool {
            self.inner.async_reads_busy()
        }

        fn pending_read_fd(&self, slot: usize) -> Option<std::os::fd::RawFd> {
            self.inner.pending_read_fd(slot)
        }

        fn advance_pending_read(&mut self, slot: usize) -> Result<bool, StoreError> {
            self.inner.advance_pending_read(slot)
        }

        fn next_hedge_deadline(&self) -> Option<std::time::Instant> {
            self.inner.next_hedge_deadline()
        }

        fn issue_due_hedges(&mut self, now: std::time::Instant) {
            self.inner.issue_due_hedges(now);
        }
    };
}

mod bloom;
mod cache;
mod disk;
#[cfg(test)]
mod memory;
mod object;
mod sst;
mod tiered;
mod value;

pub(crate) mod lz4;

#[cfg(test)]
pub(crate) use memory::MemoryBlockStore;
pub(crate) use object::OwnedObjectStore;
pub(crate) use sst::copy_pax_v2_row_from_extents;
pub(crate) use sst::{
    DataBlockLookahead, DataBlockRef, PaxLayout, SstCursor, SstHandle, SstKey, SstReader,
};
pub(crate) use sst::{MAX_ASSEMBLED, MAX_INLINE_ROW, SstError, SstWriter};
pub(crate) use sst::{
    block_keys_at, copy_block_entry_at, data_block_total, decode_data_block, locate_data_block_ref,
    locate_data_block_with_next, pax_layout, prefetch_data_block, read_data_block_raw_ref,
    take_prefetched_index_first_data,
};
pub(crate) use tiered::TieredStore;
pub(crate) use tiered::{StackPlan, build as build_tiers};
pub(crate) use value::{
    VALUE_INDEX_KEY_MAX, ValueIndexHandle, ValueIndexReader, ValueIndexWriter, walk_value_roster,
};

use crate::wal::crc32c::crc32c;

/// Bytes in a block, header included. Large enough that an object-storage GET
/// is worth its round trip, small enough that reading one row does not drag a
/// megabyte behind it. The read path is ranged, so this is the granularity a
/// cache miss costs.
pub(crate) const BLOCK_SIZE: usize = 256 * 1024;

/// `checksum` u32 | `block_type` u8 | `reserved` [u8; 3] | `lsn` u64 |
/// `len` u32 | `block_id` [u8; 32].
pub(crate) const HEADER_LEN: usize = 4 + 1 + 3 + 8 + 4 + 32;

/// The largest payload one block can carry.
pub(crate) const MAX_PAYLOAD: usize = BLOCK_SIZE - HEADER_LEN;

/// What a block holds. Stored in the header so a block found on its own — in a
/// cache, in a bucket listing, during recovery — says what it is without a
/// catalog to consult.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum BlockType {
    /// Sorted rows: the leaf of an SST.
    SstData = 1,
    /// The sparse key index of an SST.
    SstIndex = 2,
    /// An SST's bloom filter.
    SstFilter = 3,
    /// One record of the manifest log.
    ManifestLog = 4,
    /// A WAL segment shipped to the bucket.
    WalSegment = 5,
    /// An SST's complete block roster (every identity the SST comprises,
    /// itself included last as the index/filter are known by then) — what
    /// garbage collection walks instead of the data blocks themselves.
    SstRoster = 6,
    /// A sorted-row data block whose payload is LZ4-block-compressed (the
    /// hand-rolled [`lz4`]); the writer keeps whichever of raw/compressed
    /// is smaller, so both types coexist in one SST.
    SstDataLz4 = 7,
    /// Commit-LSN-versioned sorted rows.
    SstDataV2 = 8,
    /// LZ4-compressed commit-LSN-versioned sorted rows.
    SstDataV2Lz4 = 9,
    /// Sparse index over `(rowid, commit_lsn)` keys.
    SstIndexV2 = 10,
    /// Encoded secondary-index keys with row identity and commit LSN.
    ValueIndexData = 11,
    /// Complete immutable-block roster for one secondary-index generation.
    ValueIndexRoster = 12,
    /// Commit-LSN-versioned rows laid out as one self-describing PAX group.
    SstDataPaxV1 = 13,
    /// Immutable container holding several independently checksummed PAX data
    /// blocks. SST index entries name their byte extents inside this object.
    SstPackedContainerV1 = 14,
    /// Commit-LSN-versioned PAX descriptor: row keys and null maps name
    /// independently verified physical column extents.
    SstDataPaxV2 = 15,
    /// One physical PAX column extent named by an [`SstDataPaxV2`] descriptor.
    SstDataPaxColumnV1 = 16,
}

impl BlockType {
    fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            1 => BlockType::SstData,
            2 => BlockType::SstIndex,
            3 => BlockType::SstFilter,
            4 => BlockType::ManifestLog,
            5 => BlockType::WalSegment,
            6 => BlockType::SstRoster,
            7 => BlockType::SstDataLz4,
            8 => BlockType::SstDataV2,
            9 => BlockType::SstDataV2Lz4,
            10 => BlockType::SstIndexV2,
            11 => BlockType::ValueIndexData,
            12 => BlockType::ValueIndexRoster,
            13 => BlockType::SstDataPaxV1,
            14 => BlockType::SstPackedContainerV1,
            15 => BlockType::SstDataPaxV2,
            16 => BlockType::SstDataPaxColumnV1,
            _ => return None,
        })
    }
}

/// A block's name: the SHA-256 of its payload. Two blocks with the same
/// contents have the same identity by construction, which is what makes a write
/// idempotent and a retry harmless.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) struct BlockId(pub(crate) [u8; 32]);

impl BlockId {
    pub(crate) fn of(payload: &[u8]) -> Self {
        BlockId(crate::crypto::sha256::sha256(payload))
    }

    /// The object-storage key for this block, lowercase hex. Written into a
    /// caller-provided buffer, which must hold 64 bytes.
    pub(crate) fn write_key(&self, out: &mut [u8; 64]) {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        for (i, byte) in self.0.iter().enumerate() {
            out[i * 2] = HEX[(byte >> 4) as usize];
            out[i * 2 + 1] = HEX[(byte & 0xf) as usize];
        }
    }
}

/// Why a block could not be read back as itself. Every one of these is fatal to
/// the read that raised it: a block is either exactly what it claims or it is
/// not usable, and there is no partial answer to give.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum BlockError {
    /// Fewer bytes than a header, or fewer than the header says to expect.
    Truncated,
    /// The CRC over the block does not match — the bytes changed after writing.
    ChecksumMismatch,
    /// The payload does not hash to the identity in the header, so this is not
    /// the block that was asked for even if it is a valid block.
    IdentityMismatch,
    /// A payload longer than a block can hold.
    TooLarge,
    /// A `block_type` this build does not know.
    UnknownType,
    /// A compressed payload that does not decode. The checksum passed, so
    /// this is a writer's bug, not transit damage.
    Payload,
}

/// Writes `payload` into `out` as a complete block and returns its identity and
/// the number of bytes written. `out` must be at least `HEADER_LEN +
/// payload.len()`; nothing is allocated.
pub(crate) fn encode(
    payload: &[u8],
    block_type: BlockType,
    lsn: u64,
    out: &mut [u8],
) -> Result<(BlockId, usize), BlockError> {
    if payload.len() > MAX_PAYLOAD {
        return Err(BlockError::TooLarge);
    }
    let total = HEADER_LEN + payload.len();
    if out.len() < total {
        return Err(BlockError::Truncated);
    }
    let id = BlockId::of(payload);
    // The checksum covers everything after itself, so it is written last.
    out[4] = block_type as u8;
    out[5..8].fill(0);
    out[8..16].copy_from_slice(&lsn.to_le_bytes());
    out[16..20].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    out[20..HEADER_LEN].copy_from_slice(&id.0);
    out[HEADER_LEN..total].copy_from_slice(payload);
    let checksum = crc32c(&out[4..total]);
    out[0..4].copy_from_slice(&checksum.to_le_bytes());
    Ok((id, total))
}

/// A block read back out of its bytes, borrowing the payload in place.
pub(crate) struct Block<'a> {
    pub(crate) id: BlockId,
    pub(crate) block_type: BlockType,
    #[cfg(test)]
    pub(crate) lsn: u64,
    pub(crate) payload: &'a [u8],
}

/// Reads a block, verifying it. `verify_identity` re-hashes the payload, which
/// is what a block arriving from object storage or a cache needs; a block
/// already trusted in memory can skip that cost and rely on the CRC, which
/// still catches damage.
pub(crate) fn decode(bytes: &[u8], verify_identity: bool) -> Result<Block<'_>, BlockError> {
    if bytes.len() < HEADER_LEN {
        return Err(BlockError::Truncated);
    }
    let len = u32::from_le_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]) as usize;
    let total = HEADER_LEN + len;
    if len > MAX_PAYLOAD || bytes.len() < total {
        return Err(BlockError::Truncated);
    }
    let stored = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    if crc32c(&bytes[4..total]) != stored {
        return Err(BlockError::ChecksumMismatch);
    }
    let Some(block_type) = BlockType::from_code(bytes[4]) else {
        return Err(BlockError::UnknownType);
    };
    let mut id = [0u8; 32];
    id.copy_from_slice(&bytes[20..HEADER_LEN]);
    let payload = &bytes[HEADER_LEN..total];
    if verify_identity && BlockId::of(payload).0 != id {
        return Err(BlockError::IdentityMismatch);
    }
    Ok(Block {
        id: BlockId(id),
        block_type,
        #[cfg(test)]
        lsn: u64::from_le_bytes([
            bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
        ]),
        payload,
    })
}

/// Where blocks live. The seam the tiered cache, the local grid and the bucket
/// all sit behind, so the layers above never learn which one answered.
///
/// `get` fills a caller-provided buffer rather than returning one: the buffer
/// belongs to a pool reserved at startup, and a store that allocated its own
/// would put the budget back in the hands of whatever happens to be reading.
pub(crate) trait BlockStore {
    /// Stores a block and returns its identity. Storing a block that is already
    /// present is not an error and not a second copy — the identity is the
    /// content, so the write has already happened.
    fn put(
        &mut self,
        payload: &[u8],
        block_type: BlockType,
        lsn: u64,
    ) -> Result<BlockId, StoreError>;

    /// Reads the block named `id` into `into`, returning its payload length
    /// and its type — the type travels because a cached compressed block
    /// stays compressed, and only the type says so. Verifies the block; a
    /// mismatch is an error, never a shorter answer.
    fn get(&mut self, id: &BlockId, into: &mut [u8]) -> Result<(usize, BlockType), StoreError>;

    #[cfg(test)]
    fn contains(&mut self, _id: &BlockId) -> Result<bool, StoreError> {
        Err(StoreError::Unavailable)
    }

    /// Reads one independently framed logical block from an immutable packed
    /// container. The default is correct for in-memory stores; object-backed
    /// implementations override it with one ranged GET. `expected` is the
    /// logical block identity, so a container extent is never trusted merely
    /// because its enclosing object was found.
    fn get_packed(
        &mut self,
        container: &BlockId,
        offset: usize,
        length: usize,
        expected: &BlockId,
        into: &mut [u8],
        scratch: &mut [u8],
    ) -> Result<(usize, BlockType), StoreError> {
        let (container_len, container_type) = self.get(container, scratch)?;
        if container_type != BlockType::SstPackedContainerV1
            || offset
                .checked_add(length)
                .is_none_or(|end| end > container_len)
        {
            return Err(StoreError::Corrupt(BlockError::Truncated));
        }
        decode_packed_block(&scratch[offset..offset + length], expected, into)
    }

    /// Enables non-blocking object reads for this stack. The server owns the
    /// resulting socket readiness registration.
    fn enable_async_gets(&mut self) {}

    fn disable_async_gets(&mut self) {}

    /// Whether this stack currently turns GETs into reactor-owned requests.
    /// A scan uses this to schedule lookahead only when the request can remain
    /// in flight while it consumes its current block.
    fn async_gets_enabled(&self) -> bool {
        false
    }

    /// Schedules a read without transferring its completed body to a caller.
    /// The later `get` for `id` owns that transfer, so a prefetch cannot lose a
    /// response when the cache stack has no resident tier.
    fn prefetch(&mut self, _id: &BlockId) -> Result<PrefetchState, StoreError> {
        Ok(PrefetchState::Unavailable)
    }

    /// Transfers a completed speculative response into `into`. `None` means
    /// this request is still pending or was never scheduled; ordinary demand
    /// reads retain ownership of every other response.
    fn take_prefetch(
        &mut self,
        _id: &BlockId,
        _into: &mut [u8],
    ) -> Result<Option<(usize, BlockType)>, StoreError> {
        Ok(None)
    }

    /// Number of independently connected asynchronous read slots.
    fn async_read_slots(&self) -> usize {
        0
    }

    /// Whether any asynchronous slot still belongs to a caller, including a
    /// completed body or terminal error awaiting that caller's retry.
    fn async_reads_busy(&self) -> bool {
        false
    }

    /// The socket of an in-flight object read, if this store has one.
    fn pending_read_fd(&self, _slot: usize) -> Option<std::os::fd::RawFd> {
        None
    }

    /// Advances an in-flight object read. `Ok(false)` means it remains
    /// pending; `Ok(true)` means it completed and its caller may retry.
    fn advance_pending_read(&mut self, _slot: usize) -> Result<bool, StoreError> {
        Ok(false)
    }

    /// The next configured p95 hedge deadline for a pending object read.
    fn next_hedge_deadline(&self) -> Option<std::time::Instant> {
        None
    }

    /// Starts every due duplicate GET that fits in the startup-bounded slot
    /// pool. A hedge is best-effort scheduling; its winner still transfers
    /// through the ordinary demand read.
    fn issue_due_hedges(&mut self, _now: std::time::Instant) {}

    /// Cumulative provider-neutral I/O counters for this store stack.
    ///
    /// Every cache layer adds its own counters to the slower layers it wraps,
    /// so callers can distinguish a RAM hit from a disk hit and a durable-tier
    /// request without knowing which provider implements that durable tier.
    /// Stores with no observable tier (the in-memory test store, for example)
    /// keep the all-zero default.
    fn io_stats(&self) -> BlockIoStats {
        BlockIoStats::default()
    }
}

/// Validates a framed logical block carried by a packed-container extent and
/// copies its payload into a caller-owned cache/read buffer.
pub(crate) fn decode_packed_block(
    bytes: &[u8],
    expected: &BlockId,
    into: &mut [u8],
) -> Result<(usize, BlockType), StoreError> {
    let block = decode(bytes, true)?;
    if block.id != *expected {
        return Err(StoreError::Corrupt(BlockError::IdentityMismatch));
    }
    if into.len() < block.payload.len() {
        return Err(StoreError::BufferTooSmall);
    }
    into[..block.payload.len()].copy_from_slice(block.payload);
    Ok((block.payload.len(), block.block_type))
}

/// Cumulative traffic through the tiered block stack.
///
/// These are execution telemetry, not durable database state. Losing them on
/// restart changes only `EXPLAIN (ANALYZE, BUFFERS)` output and planner
/// calibration, never query results or durability.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(crate) struct BlockIoStats {
    pub(crate) ram_hits: u64,
    pub(crate) ram_misses: u64,
    pub(crate) disk_hits: u64,
    pub(crate) disk_misses: u64,
    pub(crate) object_gets: u64,
    /// Completed durable-block response bodies. This differs from issued GETs:
    /// a hedge or failed request may never deliver a body.
    pub(crate) object_read_completions: u64,
    /// Payload bytes carried by completed durable-block GET responses.
    pub(crate) object_read_bytes: u64,
    /// Elapsed wall time awaiting completed durable-block GET responses.
    /// Telemetry calibrates future plans only; it is not durable state.
    pub(crate) object_read_micros: u64,
    /// Readiness events observed after their fixed request slot was released.
    /// Kernel readiness is level-triggered, so this is expected lifecycle
    /// telemetry rather than a failed durable read.
    pub(crate) object_read_stale_events: u64,
    /// Speculative reads accepted into an owned fixed GET slot.
    pub(crate) object_prefetch_scheduled: u64,
    /// Speculative reads that reused an already-owned request or cache body.
    pub(crate) object_prefetch_reused: u64,
    /// Speculative reads refused because every fixed GET slot was owned.
    pub(crate) object_prefetch_saturated: u64,
    pub(crate) object_puts: u64,
}

impl BlockIoStats {
    pub(crate) fn saturating_sub(self, earlier: Self) -> Self {
        Self {
            ram_hits: self.ram_hits.saturating_sub(earlier.ram_hits),
            ram_misses: self.ram_misses.saturating_sub(earlier.ram_misses),
            disk_hits: self.disk_hits.saturating_sub(earlier.disk_hits),
            disk_misses: self.disk_misses.saturating_sub(earlier.disk_misses),
            object_gets: self.object_gets.saturating_sub(earlier.object_gets),
            object_read_completions: self
                .object_read_completions
                .saturating_sub(earlier.object_read_completions),
            object_read_bytes: self
                .object_read_bytes
                .saturating_sub(earlier.object_read_bytes),
            object_read_micros: self
                .object_read_micros
                .saturating_sub(earlier.object_read_micros),
            object_read_stale_events: self
                .object_read_stale_events
                .saturating_sub(earlier.object_read_stale_events),
            object_prefetch_scheduled: self
                .object_prefetch_scheduled
                .saturating_sub(earlier.object_prefetch_scheduled),
            object_prefetch_reused: self
                .object_prefetch_reused
                .saturating_sub(earlier.object_prefetch_reused),
            object_prefetch_saturated: self
                .object_prefetch_saturated
                .saturating_sub(earlier.object_prefetch_saturated),
            object_puts: self.object_puts.saturating_sub(earlier.object_puts),
        }
    }
}

/// The scheduler's explicit disposition for one speculative block request.
/// A full bounded pool is not an error for a lookahead optimization, but it is
/// observable rather than silently treated as if the request had been issued.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PrefetchState {
    Scheduled,
    Reused,
    Saturated,
    /// This stack cannot retain an asynchronous response. Callers must only
    /// ask for prefetch after checking [`BlockStore::async_gets_enabled`].
    Unavailable,
}

/// A block store's failures, kept separate from [`BlockError`] so a caller can
/// tell "the bytes are wrong" from "the bytes did not arrive".
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum StoreError {
    /// The block is not there.
    NotFound,
    /// The block is there and is not valid.
    Corrupt(BlockError),
    /// The caller's buffer is too small for the block.
    BufferTooSmall,
    /// The backing store could not be reached or refused the operation.
    Unavailable,
    /// A non-blocking fetch is in progress; retry later.
    NotReady,
}

impl From<BlockError> for StoreError {
    fn from(e: BlockError) -> Self {
        StoreError::Corrupt(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(payload: &[u8], block_type: BlockType, lsn: u64) {
        let mut buffer = [0u8; BLOCK_SIZE];
        let (id, n) = encode(payload, block_type, lsn, &mut buffer).expect("encodes");
        assert_eq!(n, HEADER_LEN + payload.len());
        let block = decode(&buffer[..n], true).expect("decodes");
        assert_eq!(block.payload, payload);
        assert_eq!(block.block_type, block_type);
        assert_eq!(block.lsn, lsn);
        assert_eq!(block.id, id);
    }

    #[test]
    fn round_trips_every_block_type() {
        for (i, t) in [
            BlockType::SstData,
            BlockType::SstIndex,
            BlockType::SstFilter,
            BlockType::ManifestLog,
            BlockType::WalSegment,
            BlockType::SstRoster,
            BlockType::SstDataLz4,
            BlockType::SstDataV2,
            BlockType::SstDataV2Lz4,
            BlockType::SstDataPaxV1,
            BlockType::SstPackedContainerV1,
            BlockType::SstDataPaxV2,
            BlockType::SstDataPaxColumnV1,
            BlockType::SstIndexV2,
            BlockType::ValueIndexData,
            BlockType::ValueIndexRoster,
        ]
        .into_iter()
        .enumerate()
        {
            round_trip(b"the quick brown fox", t, i as u64 + 1);
        }
    }

    #[test]
    fn round_trips_the_edges() {
        round_trip(b"", BlockType::SstData, 0);
        round_trip(&[0xab; 1], BlockType::SstData, u64::MAX);
        let full = [0x5au8; MAX_PAYLOAD];
        round_trip(&full, BlockType::SstData, 7);
    }

    #[test]
    fn identity_is_the_content() {
        // The same bytes are the same block however they were produced, which
        // is what makes a repeated write idempotent.
        assert_eq!(BlockId::of(b"abc"), BlockId::of(b"abc"));
        assert_ne!(BlockId::of(b"abc"), BlockId::of(b"abd"));
        let mut a = [0u8; BLOCK_SIZE];
        let mut b = [0u8; BLOCK_SIZE];
        let (id_a, _) = encode(b"payload", BlockType::SstData, 1, &mut a).unwrap();
        let (id_b, _) = encode(b"payload", BlockType::SstData, 99, &mut b).unwrap();
        assert_eq!(id_a, id_b, "identity is the payload, not the metadata");
    }

    #[test]
    fn a_flipped_byte_fails_loudly() {
        let payload = b"a block that will be damaged";
        let mut buffer = [0u8; BLOCK_SIZE];
        let (_, n) = encode(payload, BlockType::SstData, 5, &mut buffer).unwrap();
        // Every byte, header and payload alike, is covered.
        for at in 0..n {
            let mut damaged = buffer;
            damaged[at] ^= 0x01;
            assert!(
                decode(&damaged[..n], true).is_err(),
                "a flipped byte at {at} decoded as a valid block"
            );
        }
    }

    #[test]
    fn a_substituted_payload_is_not_the_named_block() {
        // Re-checksummed damage passes the CRC, so the identity hash is what
        // stands between a bucket returning the wrong object and the caller
        // believing it.
        let mut buffer = [0u8; BLOCK_SIZE];
        let (_, n) = encode(b"the real payload", BlockType::SstData, 1, &mut buffer).unwrap();
        buffer[HEADER_LEN..n].copy_from_slice(b"a fake  payload!");
        let checksum = crc32c(&buffer[4..n]);
        buffer[0..4].copy_from_slice(&checksum.to_le_bytes());
        assert_eq!(
            decode(&buffer[..n], false).map(|b| b.payload),
            Ok(&b"a fake  payload!"[..])
        );
        assert_eq!(
            decode(&buffer[..n], true).err(),
            Some(BlockError::IdentityMismatch)
        );
    }

    #[test]
    fn truncation_and_overlong_payloads_are_refused() {
        let mut buffer = [0u8; BLOCK_SIZE];
        let (_, n) = encode(b"short", BlockType::SstData, 1, &mut buffer).unwrap();
        for short in 0..n {
            assert!(
                decode(&buffer[..short], true).is_err(),
                "accepted {short} of {n} bytes"
            );
        }
        let mut small = [0u8; 8];
        assert_eq!(
            encode(b"x", BlockType::SstData, 1, &mut small).err(),
            Some(BlockError::Truncated)
        );
        let too_big = [0u8; MAX_PAYLOAD + 1];
        let mut out = [0u8; BLOCK_SIZE + 64];
        assert_eq!(
            encode(&too_big, BlockType::SstData, 1, &mut out).err(),
            Some(BlockError::TooLarge)
        );
    }

    #[test]
    fn an_unknown_block_type_is_refused() {
        let mut buffer = [0u8; BLOCK_SIZE];
        let (_, n) = encode(b"payload", BlockType::SstData, 1, &mut buffer).unwrap();
        buffer[4] = 200;
        let checksum = crc32c(&buffer[4..n]);
        buffer[0..4].copy_from_slice(&checksum.to_le_bytes());
        assert_eq!(
            decode(&buffer[..n], true).err(),
            Some(BlockError::UnknownType)
        );
    }

    #[test]
    fn the_key_is_lowercase_hex_of_the_identity() {
        let id = BlockId::of(b"abc");
        let mut key = [0u8; 64];
        id.write_key(&mut key);
        // SHA-256("abc"), the standard vector.
        assert_eq!(
            core::str::from_utf8(&key).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
