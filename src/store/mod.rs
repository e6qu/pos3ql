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

// Reached by this module's own tests and by the stages that build on it — the
// tiered cache, the SST writer, the manifest log — but not yet by the running
// server, so a --lib build sees it as dead while a --tests build does not.
// `allow` rather than `expect` for that reason, as `prng` has it: an `expect`
// here would be unfulfilled in the test build and fail it.
#![allow(dead_code)]

mod cache;
mod disk;
mod memory;
mod object;
mod tiered;

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
}

impl BlockType {
    fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            1 => BlockType::SstData,
            2 => BlockType::SstIndex,
            3 => BlockType::SstFilter,
            4 => BlockType::ManifestLog,
            5 => BlockType::WalSegment,
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
        BlockId(crate::s3::sha256::sha256(payload))
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

    /// Reads the block named `id` into `into`, returning its payload length.
    /// Verifies the block; a mismatch is an error, never a shorter answer.
    fn get(&mut self, id: &BlockId, into: &mut [u8]) -> Result<usize, StoreError>;

    /// Whether the block is present without reading it.
    fn contains(&mut self, id: &BlockId) -> Result<bool, StoreError>;
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
        assert_eq!(decode(&buffer[..n], false).map(|b| b.payload), Ok(&b"a fake  payload!"[..]));
        assert_eq!(decode(&buffer[..n], true).err(), Some(BlockError::IdentityMismatch));
    }

    #[test]
    fn truncation_and_overlong_payloads_are_refused() {
        let mut buffer = [0u8; BLOCK_SIZE];
        let (_, n) = encode(b"short", BlockType::SstData, 1, &mut buffer).unwrap();
        for short in 0..n {
            assert!(decode(&buffer[..short], true).is_err(), "accepted {short} of {n} bytes");
        }
        let mut small = [0u8; 8];
        assert_eq!(encode(b"x", BlockType::SstData, 1, &mut small).err(), Some(BlockError::Truncated));
        let too_big = [0u8; MAX_PAYLOAD + 1];
        let mut out = [0u8; BLOCK_SIZE + 64];
        assert_eq!(encode(&too_big, BlockType::SstData, 1, &mut out).err(), Some(BlockError::TooLarge));
    }

    #[test]
    fn an_unknown_block_type_is_refused() {
        let mut buffer = [0u8; BLOCK_SIZE];
        let (_, n) = encode(b"payload", BlockType::SstData, 1, &mut buffer).unwrap();
        buffer[4] = 200;
        let checksum = crc32c(&buffer[4..n]);
        buffer[0..4].copy_from_slice(&checksum.to_le_bytes());
        assert_eq!(decode(&buffer[..n], true).err(), Some(BlockError::UnknownType));
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
