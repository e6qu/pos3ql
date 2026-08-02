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

use crate::object_store::{Client as ObjectStore, Error as ObjectError, Precondition};

use super::{BlockId, BlockIoStats, BlockStore, BlockType, HEADER_LEN, StoreError, decode, encode};

/// Blocks kept as objects under a key prefix.
pub(crate) struct ObjectBlockStore<'c> {
    client: &'c mut ObjectStore,
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
        client: &'c mut ObjectStore,
        prefix: &'static str,
        scratch: &'c mut [u8],
    ) -> Self {
        Self {
            client,
            prefix,
            scratch,
        }
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
fn store_error(e: ObjectError) -> StoreError {
    match e {
        ObjectError::Status { code: 404, .. } => StoreError::NotFound,
        ObjectError::WouldBlock => StoreError::NotReady,
        _ => StoreError::Unavailable,
    }
}

fn put_block(
    client: &mut ObjectStore,
    prefix: &str,
    scratch: &mut [u8],
    payload: &[u8],
    block_type: BlockType,
    lsn: u64,
) -> Result<BlockId, StoreError> {
    let (id, n) = encode(payload, block_type, lsn, scratch)?;
    let mut key_buffer = [0u8; 128];
    let key = key_of(prefix, &id, &mut key_buffer);
    // No precondition: the key is the content, so writing a block that is
    // already there writes the same bytes. Conditional-create would turn a
    // harmless retry into an error the caller would have to interpret.
    client
        .put(key, &scratch[..n], Precondition::None)
        .map_err(store_error)?;
    Ok(id)
}

fn get_block(
    client: &mut ObjectStore,
    prefix: &str,
    id: &BlockId,
    into: &mut [u8],
) -> Result<(usize, BlockType), StoreError> {
    let mut key_buffer = [0u8; 128];
    let key = key_of(prefix, id, &mut key_buffer);
    let result = client.get(key, None).map_err(store_error)?;
    let body = &client.body_bytes()[..result.len];
    decode_block_body(body, id, into)
}

fn decode_block_body(
    body: &[u8],
    id: &BlockId,
    into: &mut [u8],
) -> Result<(usize, BlockType), StoreError> {
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
    Ok((block.payload.len(), block.block_type))
}

/// The bucket store that owns its client and scratch — the long-lived form a
/// cache stack sits over, where a borrowed client would tangle lifetimes.
pub(crate) struct OwnedObjectStore {
    client: ObjectStore,
    prefix: &'static str,
    scratch: Vec<u8>,
    stats: BlockIoStats,
    /// Terminal result of the asynchronous read, returned by the parked
    /// statement's retry instead of starting an unrelated second request.
    pending_error: Option<StoreError>,
    pending_id: Option<BlockId>,
    ready_id: Option<BlockId>,
}

impl OwnedObjectStore {
    /// Startup-only: the scratch Vec is reserved once, before the allocator
    /// freezes, and never grows.
    pub(crate) fn new(client: ObjectStore, prefix: &'static str) -> Self {
        Self {
            client,
            prefix,
            scratch: vec![0u8; super::BLOCK_SIZE],
            stats: BlockIoStats::default(),
            pending_error: None,
            pending_id: None,
            ready_id: None,
        }
    }

    fn enable_async_gets(&mut self) {
        self.client.enable_async_gets();
    }

    fn disable_async_gets(&mut self) {
        self.client.disable_async_gets();
    }

    fn pending_read_fd(&self) -> Option<std::os::fd::RawFd> {
        self.client.pending_get_fd()
    }

    fn advance_pending_read(&mut self) -> Result<bool, StoreError> {
        match self.client.advance_get() {
            Ok(()) => {
                self.ready_id = self.pending_id.take();
                Ok(true)
            }
            Err(ObjectError::WouldBlock) => Ok(false),
            Err(error) => {
                self.client.clear_pending_get();
                self.pending_error = Some(store_error(error));
                self.pending_id = None;
                Ok(true)
            }
        }
    }
}

impl BlockStore for OwnedObjectStore {
    fn put(
        &mut self,
        payload: &[u8],
        block_type: BlockType,
        lsn: u64,
    ) -> Result<BlockId, StoreError> {
        let result = put_block(
            &mut self.client,
            self.prefix,
            &mut self.scratch,
            payload,
            block_type,
            lsn,
        );
        self.stats.object_puts = self.stats.object_puts.saturating_add(1);
        result
    }

    fn get(&mut self, id: &BlockId, into: &mut [u8]) -> Result<(usize, BlockType), StoreError> {
        if let Some(error) = self.pending_error.take() {
            return Err(error);
        }
        if self.ready_id == Some(*id) {
            self.ready_id = None;
            return decode_block_body(self.client.body_bytes(), id, into);
        }
        if self.pending_id.is_some() || self.ready_id.is_some() {
            return Err(StoreError::NotReady);
        }
        let result = get_block(&mut self.client, self.prefix, id, into);
        if matches!(result, Err(StoreError::NotReady)) {
            self.pending_id = Some(*id);
        }
        self.stats.object_gets = self.stats.object_gets.saturating_add(1);
        result
    }

    fn contains(&mut self, id: &BlockId) -> Result<bool, StoreError> {
        let mut key_buffer = [0u8; 128];
        let key = key_of(self.prefix, id, &mut key_buffer);
        self.stats.object_contains = self.stats.object_contains.saturating_add(1);
        match self.client.get(key, Some((0, HEADER_LEN as u64 - 1))) {
            Ok(_) => Ok(true),
            Err(ObjectError::Status { code: 404, .. }) => Ok(false),
            Err(e) => Err(store_error(e)),
        }
    }

    fn io_stats(&self) -> BlockIoStats {
        self.stats
    }

    fn enable_async_gets(&mut self) {
        self.enable_async_gets();
    }

    fn disable_async_gets(&mut self) {
        self.disable_async_gets();
    }

    fn pending_read_fd(&self) -> Option<std::os::fd::RawFd> {
        self.pending_read_fd()
    }

    fn advance_pending_read(&mut self) -> Result<bool, StoreError> {
        self.advance_pending_read()
    }
}

impl BlockStore for ObjectBlockStore<'_> {
    fn put(
        &mut self,
        payload: &[u8],
        block_type: BlockType,
        lsn: u64,
    ) -> Result<BlockId, StoreError> {
        put_block(
            self.client,
            self.prefix,
            self.scratch,
            payload,
            block_type,
            lsn,
        )
    }

    fn get(&mut self, id: &BlockId, into: &mut [u8]) -> Result<(usize, BlockType), StoreError> {
        get_block(self.client, self.prefix, id, into)
    }

    fn contains(&mut self, id: &BlockId) -> Result<bool, StoreError> {
        let mut key_buffer = [0u8; 128];
        let key = key_of(self.prefix, id, &mut key_buffer);
        // Only the header is fetched: presence is a property of the object, and
        // dragging the payload across to learn it would make an existence check
        // cost as much as a read.
        match self.client.get(key, Some((0, HEADER_LEN as u64 - 1))) {
            Ok(_) => Ok(true),
            Err(ObjectError::Status { code: 404, .. }) => Ok(false),
            Err(e) => Err(store_error(e)),
        }
    }
}
