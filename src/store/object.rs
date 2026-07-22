//! The object-storage backend: the bucket as the system of record.
//!
//! One object per block, named by the block's identity. That naming is what
//! makes the write path forgiving: a PUT that times out after the object landed
//! is indistinguishable from one that landed cleanly, because re-writing the
//! same block writes the same bytes to the same key. There is no read-modify-
//! write anywhere in this layer, so two writers racing on the same block agree
//! by construction rather than by locking.
//!
//! Reads verify. A block arriving from a bucket has crossed a network and a
//! service this process does not control, so the identity hash is checked as
//! well as the CRC — the one case a checksum cannot cover is being handed a
//! *different* block that is itself intact.

use crate::s3::{Precondition, S3Client, S3Error};

use super::{decode, encode, BlockId, BlockStore, BlockType, StoreError, HEADER_LEN};

/// Blocks kept as objects under a key prefix.
pub(crate) struct ObjectBlockStore<'c> {
    client: &'c mut S3Client,
    /// Prefix every block key sits under, e.g. `blocks/`. Kept short: it is
    /// paid on every request line.
    prefix: &'static str,
    /// Scratch for building one block before it is written. A block store does
    /// not allocate, so the buffer it needs to frame a block is reserved with
    /// the store.
    scratch: &'c mut [u8],
}

impl<'c> ObjectBlockStore<'c> {
    /// `scratch` must hold a whole block — `HEADER_LEN + MAX_PAYLOAD`.
    pub(crate) fn new(
        client: &'c mut S3Client,
        prefix: &'static str,
        scratch: &'c mut [u8],
    ) -> Self {
        Self { client, prefix, scratch }
    }

}

/// `<prefix><64 hex chars>`, written into a caller-provided buffer so that
/// naming a block costs nothing.
fn key_of<'k>(prefix: &str, id: &BlockId, out: &'k mut [u8; 128]) -> &'k str {
    let prefix = prefix.as_bytes();
    out[..prefix.len()].copy_from_slice(prefix);
    let mut hex = [0u8; 64];
    id.write_key(&mut hex);
    out[prefix.len()..prefix.len() + 64].copy_from_slice(&hex);
    // Both halves are ASCII by construction.
    core::str::from_utf8(&out[..prefix.len() + 64]).expect("hex key is ASCII")
}

/// A missing object is `NotFound`; everything else is `Unavailable`, because a
/// caller can retry the second and cannot conjure the first.
fn store_error(e: S3Error) -> StoreError {
    match e {
        S3Error::Status { code: 404, .. } => StoreError::NotFound,
        _ => StoreError::Unavailable,
    }
}

impl BlockStore for ObjectBlockStore<'_> {
    fn put(
        &mut self,
        payload: &[u8],
        block_type: BlockType,
        lsn: u64,
    ) -> Result<BlockId, StoreError> {
        let (id, n) = encode(payload, block_type, lsn, self.scratch)?;
        let mut key_buffer = [0u8; 128];
        let key = key_of(self.prefix, &id, &mut key_buffer);
        // No precondition: the key is the content, so writing a block that is
        // already there writes the same bytes. Conditional-create would turn a
        // harmless retry into an error the caller would have to interpret.
        self.client
            .put(key, &self.scratch[..n], Precondition::None)
            .map_err(store_error)?;
        Ok(id)
    }

    fn get(&mut self, id: &BlockId, into: &mut [u8]) -> Result<usize, StoreError> {
        let mut key_buffer = [0u8; 128];
        let key = key_of(self.prefix, id, &mut key_buffer);
        let result = self.client.get(key, None).map_err(store_error)?;
        let body = &self.client.body_bytes()[..result.len];
        // Verified against the name it was fetched under, not merely against
        // its own header — a bucket handing back a different intact block is
        // exactly what content-addressing is here to catch.
        let block = decode(body, true)?;
        if block.id != *id {
            return Err(StoreError::Corrupt(super::BlockError::IdentityMismatch));
        }
        if into.len() < block.payload.len() {
            return Err(StoreError::BufferTooSmall);
        }
        into[..block.payload.len()].copy_from_slice(block.payload);
        Ok(block.payload.len())
    }

    fn contains(&mut self, id: &BlockId) -> Result<bool, StoreError> {
        let mut key_buffer = [0u8; 128];
        let key = key_of(self.prefix, id, &mut key_buffer);
        // Only the header is fetched: presence is a property of the object, and
        // dragging the payload across to learn it would make an existence check
        // cost as much as a read.
        match self.client.get(key, Some((0, HEADER_LEN as u64 - 1))) {
            Ok(_) => Ok(true),
            Err(S3Error::Status { code: 404, .. }) => Ok(false),
            Err(e) => Err(store_error(e)),
        }
    }
}
