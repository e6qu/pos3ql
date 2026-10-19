//! The object-storage backend: the namespace is the system of record.
//!
//! One object per block, named by the block's identity. That naming is what
//! makes the write path forgiving: a PUT that times out after the object landed
//! is indistinguishable from one that landed cleanly, because re-writing the
//! same block writes the same bytes to the same key. There is no read-modify-
//! write anywhere in this layer, so two writers racing on the same block agree
//! by construction rather than by locking.
//!
//! Reads verify. A block arriving from the namespace has crossed a network and a
//! service this process does not control, so the identity hash is checked as
//! well as the CRC — the one case a checksum cannot cover is being handed a
//! *different* block that is itself intact.

use crate::mem::budget::Budget;
use crate::mem::fixed_vec::FixedVec;
use crate::object_store::{ByteRange, Client as ObjectStore, Error as ObjectError, Precondition};
use std::time::{Duration, Instant};

use super::{
    BlockId, BlockIoStats, BlockStore, BlockType, HEADER_LEN, PrefetchState, ReadOwner, StoreError,
    decode, decode_packed_block, encode,
};

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

struct PackedBlockRange {
    range: ByteRange,
    length: usize,
}

impl PackedBlockRange {
    fn new(offset: usize, length: usize) -> Result<Self, StoreError> {
        let range = HEADER_LEN
            .checked_add(offset)
            .and_then(|start| {
                start
                    .checked_add(length)
                    .and_then(|end| end.checked_sub(1))
                    .and_then(|end| ByteRange::new(start as u64, end as u64).ok())
            })
            .ok_or(StoreError::Corrupt(super::BlockError::Truncated))?;
        Ok(Self { range, length })
    }
}

enum ObjectBlockRead {
    Pending,
    Failed(StoreError),
    Complete {
        bytes: usize,
        result: Result<(usize, BlockType), StoreError>,
    },
}

fn get_block(
    client: &mut ObjectStore,
    prefix: &str,
    id: &BlockId,
    into: &mut [u8],
) -> ObjectBlockRead {
    let mut key_buffer = [0u8; 128];
    let key = key_of(prefix, id, &mut key_buffer);
    match client.get(key, None) {
        Ok(response) => ObjectBlockRead::Complete {
            bytes: response.len,
            result: decode_block_body(&client.body_bytes()[..response.len], id, into),
        },
        Err(ObjectError::WouldBlock) => ObjectBlockRead::Pending,
        Err(error) => ObjectBlockRead::Failed(store_error(error)),
    }
}

fn get_packed_block(
    client: &mut ObjectStore,
    prefix: &str,
    container: &BlockId,
    range: PackedBlockRange,
    expected: &BlockId,
    into: &mut [u8],
) -> ObjectBlockRead {
    let mut key_buffer = [0u8; 128];
    let key = key_of(prefix, container, &mut key_buffer);
    match client.get(key, Some(range.range)) {
        Ok(response) => ObjectBlockRead::Complete {
            bytes: response.len,
            result: if response.len == range.length {
                decode_packed_block(client.body_bytes(), expected, into)
            } else {
                Err(StoreError::Corrupt(super::BlockError::Truncated))
            },
        },
        Err(ObjectError::WouldBlock) => ObjectBlockRead::Pending,
        Err(error) => ObjectBlockRead::Failed(store_error(error)),
    }
}

fn decode_block_body(
    body: &[u8],
    id: &BlockId,
    into: &mut [u8],
) -> Result<(usize, BlockType), StoreError> {
    // Verified against the name it was fetched under, not merely against
    // its own header — a namespace handing back a different intact block is
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum ObjectReadMode {
    Atomic,
    Restartable,
}

/// The namespace store that owns its client and scratch — the long-lived form a
/// cache stack sits over, where a borrowed client would tangle lifetimes.
pub(crate) struct OwnedObjectStore {
    atomic_client: ObjectStore,
    slots: FixedVec<ObjectReadSlot>,
    prefix: &'static str,
    scratch: Vec<u8>,
    stats: BlockIoStats,
    read_mode: ObjectReadMode,
    hedge_after: Option<Duration>,
    read_owner: Option<ReadOwner>,
    read_capacity_generation: u64,
}

struct ObjectReadSlot {
    client: ObjectStore,
    pending_id: Option<BlockId>,
    ready_id: Option<BlockId>,
    error_id: Option<BlockId>,
    speculative: bool,
    packed: bool,
    pending_error: Option<StoreError>,
    started_at: Option<Instant>,
    hedge_issued: bool,
    owners: FixedVec<ReadOwner>,
}

impl OwnedObjectStore {
    pub(crate) fn budget_bytes(config: &crate::config::Config) -> usize {
        config
            .object_store_get_slots
            .saturating_add(1)
            .saturating_mul(ObjectStore::budget_bytes(config))
            .saturating_add(config.object_store_get_slots.saturating_mul(
                core::mem::size_of::<ObjectReadSlot>()
                    + Self::owner_capacity(config) * core::mem::size_of::<ReadOwner>(),
            ))
            .saturating_add(super::BLOCK_SIZE)
    }

    fn owner_capacity(config: &crate::config::Config) -> usize {
        (config.max_connections as usize)
            .saturating_add(config.max_subscriptions)
            .saturating_add(1)
    }

    /// Startup-only: the scratch Vec is reserved once, before the allocator
    /// freezes, and never grows.
    pub(crate) fn new(
        config: &crate::config::Config,
        budget: &mut Budget,
        prefix: &'static str,
    ) -> Result<Self, crate::object_store::SetupError> {
        if config.object_store_get_slots == 0 {
            return Err(crate::object_store::SetupError::Capacity(
                "object_store_get_slots must be at least 1",
            ));
        }
        // A completed async read retains its response until its consumer
        // verifies it. Atomic reads and writes use another client so they
        // cannot replace a response still owned by a suspended reader.
        let atomic_client = ObjectStore::new(config, budget)?;
        let mut slots = FixedVec::new(
            budget,
            "object_store_get_slots",
            config.object_store_get_slots,
        )
        .map_err(crate::object_store::SetupError::Budget)?;
        for _ in 0..config.object_store_get_slots {
            slots
                .push(ObjectReadSlot {
                    client: ObjectStore::new(config, budget)?,
                    pending_id: None,
                    ready_id: None,
                    error_id: None,
                    speculative: false,
                    packed: false,
                    pending_error: None,
                    started_at: None,
                    hedge_issued: false,
                    owners: FixedVec::new(
                        budget,
                        "object read consumers",
                        Self::owner_capacity(config),
                    )
                    .map_err(crate::object_store::SetupError::Budget)?,
                })
                .expect("object read slots sized from configuration");
        }
        budget
            .draw(super::BLOCK_SIZE, "object_store_block_scratch")
            .map_err(crate::object_store::SetupError::Budget)?;
        Ok(Self {
            atomic_client,
            slots,
            prefix,
            scratch: vec![0u8; super::BLOCK_SIZE],
            stats: BlockIoStats::default(),
            read_mode: ObjectReadMode::Atomic,
            read_owner: None,
            read_capacity_generation: 0,
            hedge_after: (config.object_store_hedge_after_ms != 0)
                .then(|| Duration::from_millis(config.object_store_hedge_after_ms)),
        })
    }

    pub(crate) fn replace_credentials(&mut self, credentials: crate::object_store::Credentials) {
        self.atomic_client.replace_credentials(credentials);
        for slot in self.slots.as_mut_slice() {
            debug_assert!(slot.pending_id.is_none());
            slot.client.replace_credentials(credentials);
        }
    }

    fn enable_async_gets(&mut self) {
        for slot in self.slots.as_mut_slice() {
            slot.client.enable_async_gets();
        }
        self.read_mode = ObjectReadMode::Restartable;
    }

    fn finish_read(
        &mut self,
        started: Instant,
        read: ObjectBlockRead,
    ) -> Result<(usize, BlockType), StoreError> {
        let result = match read {
            ObjectBlockRead::Pending => Err(StoreError::NotReady),
            ObjectBlockRead::Failed(error) => Err(error),
            ObjectBlockRead::Complete { bytes, result } => {
                self.record_completed_read(started, bytes);
                result
            }
        };
        self.stats.object_gets = self.stats.object_gets.saturating_add(1);
        result
    }

    fn record_completed_read(&mut self, started: Instant, bytes: usize) {
        self.stats.object_read_completions = self.stats.object_read_completions.saturating_add(1);
        self.stats.object_read_bytes = self.stats.object_read_bytes.saturating_add(bytes as u64);
        self.stats.object_read_micros = self
            .stats
            .object_read_micros
            .saturating_add(started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64);
    }

    fn disable_async_gets(&mut self) {
        // Atomic misses use the separate synchronous client. An existing
        // reactor request keeps its socket, body, error and demand ownership.
        self.read_mode = ObjectReadMode::Atomic;
    }

    fn pending_read_fd(&self, slot: usize) -> Option<std::os::fd::RawFd> {
        self.slots.get(slot)?.client.pending_fd()
    }

    fn advance_pending_read(&mut self, slot: usize) -> Result<bool, StoreError> {
        let (result, completed) = {
            let slot = self.slots.get_mut(slot).expect("reactor slot is bounded");
            if slot.pending_id.is_none() {
                if slot.ready_id.is_none() && slot.error_id.is_none() {
                    // A readiness event can remain queued after a hedge winner
                    // releases its sibling. The slot state, not the stale
                    // kernel event, authoritatively decides whether a GET is
                    // still live; count it so the lifecycle remains visible.
                    self.stats.object_read_stale_events =
                        self.stats.object_read_stale_events.saturating_add(1);
                    return Ok(false);
                }
                return Ok(true);
            }
            match slot.client.advance_get() {
                Ok(_) => {
                    let started = slot
                        .started_at
                        .take()
                        .expect("pending GET has a start time");
                    let bytes = slot.client.body_bytes().len();
                    slot.ready_id = slot.pending_id.take();
                    (Ok(true), Some((started, bytes)))
                }
                Err(ObjectError::WouldBlock) => (Ok(false), None),
                Err(error) => {
                    slot.client.cancel_request();
                    slot.pending_error = Some(store_error(error));
                    slot.error_id = slot.pending_id.take();
                    slot.started_at = None;
                    (Ok(true), None)
                }
            }
        };
        if let Some((started, bytes)) = completed {
            self.record_completed_read(started, bytes);
        }
        result
    }

    fn slot_is_free(slot: &ObjectReadSlot) -> bool {
        slot.pending_id.is_none()
            && slot.ready_id.is_none()
            && slot.error_id.is_none()
            && slot.pending_error.is_none()
            && slot.owners.is_empty()
    }

    fn request_owner(&self) -> Result<Option<ReadOwner>, StoreError> {
        if self.read_mode == ObjectReadMode::Atomic {
            Ok(None)
        } else {
            self.read_owner
                .map(Some)
                .ok_or(StoreError::ReadOwnerRequired)
        }
    }

    fn join_owner(slot: &mut ObjectReadSlot, owner: Option<ReadOwner>) -> Result<(), StoreError> {
        if let Some(owner) = owner
            && !slot.owners.iter().any(|existing| *existing == owner)
        {
            slot.owners
                .push(owner)
                .map_err(|_| StoreError::ReadOwnersExhausted)?;
        }
        Ok(())
    }

    fn join_request(
        &mut self,
        id: BlockId,
        packed: bool,
        owner: Option<ReadOwner>,
    ) -> Result<(), StoreError> {
        for slot in self.slots.as_mut_slice() {
            if slot.packed == packed
                && (slot.pending_id == Some(id)
                    || slot.ready_id == Some(id)
                    || slot.error_id == Some(id))
            {
                Self::join_owner(slot, owner)?;
            }
        }
        Ok(())
    }

    fn clear_slot(slot: &mut ObjectReadSlot) {
        slot.client.cancel_request();
        slot.pending_id = None;
        slot.ready_id = None;
        slot.error_id = None;
        slot.pending_error = None;
        slot.started_at = None;
        slot.hedge_issued = false;
        slot.speculative = false;
        slot.packed = false;
        slot.owners.clear();
    }

    fn consume(&mut self, index: usize, owner: Option<ReadOwner>) {
        let slot = &mut self.slots[index];
        let owner = owner.or(self.read_owner);
        if let Some(owner) = owner {
            if let Some(index) = slot.owners.iter().position(|existing| *existing == owner) {
                slot.owners.swap_remove(index);
            }
        }
        if slot.owners.is_empty() {
            Self::clear_slot(slot);
            self.read_capacity_generation = self.read_capacity_generation.wrapping_add(1);
        }
    }

    fn demand_slot(&mut self, owner: Option<ReadOwner>) -> Option<&mut ObjectReadSlot> {
        let free = self.slots.iter().position(Self::slot_is_free);
        let index = free.or_else(|| {
            // A caller may abandon its own lookahead to make a demand read.
            // Never discard a result another suspended consumer still owns.
            self.slots.iter().position(|slot| {
                slot.speculative && slot.owners.len() == 1 && Some(slot.owners[0]) == owner
            })
        })?;
        if !Self::slot_is_free(&self.slots[index]) {
            Self::clear_slot(&mut self.slots[index]);
            self.read_capacity_generation = self.read_capacity_generation.wrapping_add(1);
        }
        Some(&mut self.slots[index])
    }

    fn release_siblings(&mut self, id: BlockId, winner: usize) -> Result<(), StoreError> {
        for index in 0..self.slots.len() {
            if index == winner
                || self.slots[index].packed != self.slots[winner].packed
                || (self.slots[index].pending_id != Some(id)
                    && self.slots[index].ready_id != Some(id)
                    && self.slots[index].error_id != Some(id))
            {
                continue;
            }
            let (source, destination) = if index < winner {
                let (before, after) = self.slots.as_mut_slice().split_at_mut(winner);
                (&mut before[index], &mut after[0])
            } else {
                let (before, after) = self.slots.as_mut_slice().split_at_mut(index);
                (&mut after[0], &mut before[winner])
            };
            // The winning body inherits every waiter before a duplicate socket
            // or completion is released. Cancellation removes only its caller.
            for owner in source.owners.iter() {
                Self::join_owner(destination, Some(*owner))?;
            }
            Self::clear_slot(source);
            self.read_capacity_generation = self.read_capacity_generation.wrapping_add(1);
        }
        Ok(())
    }

    fn next_read_deadline(&self) -> Option<Instant> {
        let hedge_slot_available = self.slots.iter().any(Self::slot_is_free);
        self.slots
            .iter()
            .filter_map(|slot| {
                slot.pending_id?;
                let started = slot.started_at?;
                let timeout = started + Duration::from_secs(30);
                let hedge = self
                    .hedge_after
                    .filter(|_| hedge_slot_available && !slot.hedge_issued && !slot.packed)
                    .map(|after| started + after);
                Some(hedge.map_or(timeout, |deadline| timeout.min(deadline)))
            })
            .min()
    }

    fn advance_read_deadlines(&mut self, now: Instant) -> bool {
        let mut completed = false;
        for slot in self.slots.as_mut_slice() {
            if slot.pending_id.is_some()
                && slot
                    .started_at
                    .is_some_and(|started| now >= started + Duration::from_secs(30))
            {
                slot.client.cancel_request();
                slot.error_id = slot.pending_id.take();
                slot.pending_error = Some(StoreError::ReadTimedOut);
                slot.started_at = None;
                completed = true;
            }
        }
        let Some(deadline) = self.next_read_deadline() else {
            return completed;
        };
        if deadline > now {
            return completed;
        }
        let Some(source_index) = self.slots.iter().position(|slot| {
            slot.pending_id.is_some()
                && !slot.hedge_issued
                && !slot.packed
                && slot
                    .started_at
                    .and_then(|started| {
                        self.hedge_after
                            .and_then(|after| started.checked_add(after))
                    })
                    .is_some_and(|due| due <= now)
        }) else {
            return completed;
        };
        let Some(destination_index) = self.slots.iter().position(Self::slot_is_free) else {
            return completed;
        };
        let id = self.slots[source_index]
            .pending_id
            .expect("selected pending hedge source");
        self.slots[source_index].hedge_issued = true;
        for index in 0..self.slots[source_index].owners.len() {
            let owner = self.slots[source_index].owners[index];
            Self::join_owner(&mut self.slots[destination_index], Some(owner))
                .expect("hedge slots share the same startup consumer capacity");
        }
        let mut key_buffer = [0u8; 128];
        let key = key_of(self.prefix, &id, &mut key_buffer);
        let hedge_bytes = {
            let destination = &mut self.slots[destination_index];
            match destination.client.get(key, None) {
                Ok(result) => {
                    destination.ready_id = Some(id);
                    destination.hedge_issued = true;
                    Some(result.len)
                }
                Err(ObjectError::WouldBlock) => {
                    destination.pending_id = Some(id);
                    destination.started_at = Some(now);
                    destination.hedge_issued = true;
                    None
                }
                Err(error) => {
                    destination.error_id = Some(id);
                    destination.pending_error = Some(store_error(error));
                    destination.hedge_issued = true;
                    None
                }
            }
        };
        if let Some(bytes) = hedge_bytes {
            completed = true;
            self.record_completed_read(now, bytes);
        }
        self.stats.object_gets = self.stats.object_gets.saturating_add(1);
        completed || self.slots[destination_index].error_id.is_some()
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
            &mut self.atomic_client,
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
        let owner = self.request_owner()?;
        if let Some(winner) = self
            .slots
            .iter()
            .position(|slot| slot.ready_id == Some(*id) && !slot.packed)
        {
            self.release_siblings(*id, winner)?;
            Self::join_owner(&mut self.slots[winner], owner)?;
            let result = decode_block_body(self.slots[winner].client.body_bytes(), id, into);
            self.consume(winner, owner);
            return result;
        }
        if self.read_mode == ObjectReadMode::Restartable
            && self
                .slots
                .iter()
                .any(|slot| slot.pending_id == Some(*id) && !slot.packed)
        {
            self.join_request(*id, false, owner)?;
            for slot in self.slots.as_mut_slice() {
                if slot.pending_id == Some(*id) && !slot.packed {
                    slot.speculative = false;
                }
            }
            return Err(StoreError::NotReady);
        }
        if let Some(winner) = self
            .slots
            .iter()
            .position(|slot| slot.error_id == Some(*id) && !slot.packed)
        {
            self.release_siblings(*id, winner)?;
            Self::join_owner(&mut self.slots[winner], owner)?;
            let error = self.slots[winner]
                .pending_error
                .expect("retained read failure");
            self.consume(winner, owner);
            return Err(error);
        }
        let started = Instant::now();
        let prefix = self.prefix;
        let read = if self.read_mode == ObjectReadMode::Atomic {
            get_block(&mut self.atomic_client, prefix, id, into)
        } else {
            let Some(slot) = self.demand_slot(owner) else {
                return Err(StoreError::NotReady);
            };
            Self::join_owner(slot, owner)?;
            let read = get_block(&mut slot.client, prefix, id, into);
            if matches!(&read, ObjectBlockRead::Pending) {
                slot.pending_id = Some(*id);
                slot.started_at = Some(started);
                slot.packed = false;
            } else {
                Self::clear_slot(slot);
            }
            read
        };
        self.finish_read(started, read)
    }

    fn get_packed(
        &mut self,
        container: &BlockId,
        offset: usize,
        length: usize,
        expected: &BlockId,
        into: &mut [u8],
        _scratch: &mut [u8],
    ) -> Result<(usize, BlockType), StoreError> {
        let range = PackedBlockRange::new(offset, length)?;
        let owner = self.request_owner()?;
        if let Some(winner) = self
            .slots
            .iter()
            .position(|slot| slot.ready_id == Some(*expected) && slot.packed)
        {
            Self::join_owner(&mut self.slots[winner], owner)?;
            let result = if self.slots[winner].client.body_bytes().len() != length {
                Err(StoreError::Corrupt(super::BlockError::Truncated))
            } else {
                decode_packed_block(self.slots[winner].client.body_bytes(), expected, into)
            };
            self.consume(winner, owner);
            return result;
        }
        if self.read_mode == ObjectReadMode::Restartable
            && self
                .slots
                .iter()
                .any(|slot| slot.pending_id == Some(*expected) && slot.packed)
        {
            self.join_request(*expected, true, owner)?;
            return Err(StoreError::NotReady);
        }
        if let Some(winner) = self
            .slots
            .iter()
            .position(|slot| slot.error_id == Some(*expected) && slot.packed)
        {
            Self::join_owner(&mut self.slots[winner], owner)?;
            let error = self.slots[winner]
                .pending_error
                .expect("retained packed read failure");
            self.consume(winner, owner);
            return Err(error);
        }
        let started = Instant::now();
        let prefix = self.prefix;
        let read = if self.read_mode == ObjectReadMode::Atomic {
            get_packed_block(
                &mut self.atomic_client,
                prefix,
                container,
                range,
                expected,
                into,
            )
        } else {
            let Some(slot) = self.demand_slot(owner) else {
                return Err(StoreError::NotReady);
            };
            Self::join_owner(slot, owner)?;
            let read = get_packed_block(&mut slot.client, prefix, container, range, expected, into);
            if matches!(&read, ObjectBlockRead::Pending) {
                slot.pending_id = Some(*expected);
                slot.started_at = Some(started);
                slot.packed = true;
            } else {
                Self::clear_slot(slot);
            }
            read
        };
        self.finish_read(started, read)
    }

    fn prefetch(&mut self, id: &BlockId) -> Result<PrefetchState, StoreError> {
        let owner = self.request_owner()?;
        if owner.is_none() {
            return Ok(PrefetchState::Unavailable);
        }
        if self.slots.iter().any(|slot| {
            !slot.packed
                && (slot.pending_id == Some(*id)
                    || slot.ready_id == Some(*id)
                    || slot.error_id == Some(*id))
        }) {
            self.join_request(*id, false, owner)?;
            self.stats.object_prefetch_reused = self.stats.object_prefetch_reused.saturating_add(1);
            return Ok(PrefetchState::Reused);
        }
        let mut key_buffer = [0u8; 128];
        let key = key_of(self.prefix, id, &mut key_buffer);
        let started = Instant::now();
        let completed = {
            let Some(slot) = self.slots.iter_mut().find(|slot| Self::slot_is_free(slot)) else {
                self.stats.object_prefetch_saturated =
                    self.stats.object_prefetch_saturated.saturating_add(1);
                return Ok(PrefetchState::Saturated);
            };
            Self::join_owner(slot, owner)?;
            match slot.client.get(key, None) {
                Ok(result) => {
                    slot.ready_id = Some(*id);
                    slot.speculative = true;
                    Some(result.len)
                }
                Err(ObjectError::WouldBlock) => {
                    slot.pending_id = Some(*id);
                    slot.started_at = Some(started);
                    slot.speculative = true;
                    slot.packed = false;
                    None
                }
                Err(error) => {
                    Self::clear_slot(slot);
                    return Err(store_error(error));
                }
            }
        };
        if let Some(bytes) = completed {
            self.record_completed_read(started, bytes);
        }
        self.stats.object_gets = self.stats.object_gets.saturating_add(1);
        self.stats.object_prefetch_scheduled =
            self.stats.object_prefetch_scheduled.saturating_add(1);
        Ok(PrefetchState::Scheduled)
    }

    fn take_prefetch(
        &mut self,
        id: &BlockId,
        into: &mut [u8],
    ) -> Result<Option<(usize, BlockType)>, StoreError> {
        match self.get(id, into) {
            Ok(result) => Ok(Some(result)),
            Err(StoreError::NotReady) => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn set_read_owner(&mut self, owner: Option<ReadOwner>) {
        self.read_owner = owner;
    }

    fn cancel_read_owner(&mut self, owner: ReadOwner) {
        for slot in self.slots.as_mut_slice() {
            let before = slot.owners.len();
            if let Some(index) = slot.owners.iter().position(|existing| *existing == owner) {
                slot.owners.swap_remove(index);
            }
            if before != slot.owners.len() && slot.owners.is_empty() {
                Self::clear_slot(slot);
                self.read_capacity_generation = self.read_capacity_generation.wrapping_add(1);
            }
        }
    }

    fn release_read(&mut self, id: &BlockId) {
        let Some(owner) = self.read_owner else {
            return;
        };
        for index in 0..self.slots.len() {
            let slot = &self.slots[index];
            if (slot.pending_id == Some(*id)
                || slot.ready_id == Some(*id)
                || slot.error_id == Some(*id))
                && slot.owners.iter().any(|existing| *existing == owner)
            {
                self.consume(index, Some(owner));
            }
        }
    }

    fn read_capacity_generation(&self) -> u64 {
        self.read_capacity_generation
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

    fn async_gets_enabled(&self) -> bool {
        self.read_mode == ObjectReadMode::Restartable
    }

    fn async_read_slots(&self) -> usize {
        self.slots.len()
    }

    fn async_reads_busy(&self) -> bool {
        self.slots.iter().any(|slot| slot.pending_id.is_some())
    }

    fn pending_read_fd(&self, slot: usize) -> Option<std::os::fd::RawFd> {
        self.pending_read_fd(slot)
    }

    fn pending_read_wants_write(&self, slot: usize) -> bool {
        self.slots
            .get(slot)
            .is_some_and(|slot| slot.client.pending_wants_write())
    }

    fn advance_pending_read(&mut self, slot: usize) -> Result<bool, StoreError> {
        self.advance_pending_read(slot)
    }

    fn next_read_deadline(&self) -> Option<Instant> {
        self.next_read_deadline()
    }

    fn advance_read_deadlines(&mut self, now: Instant) -> bool {
        self.advance_read_deadlines(now)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::io::Write;
    use std::net::TcpListener;
    use std::sync::mpsc;

    use super::*;

    fn network_config(port: u16, bucket: &str) -> crate::config::Config {
        let mut config = crate::config::Config::default_dev();
        config.object_store_on = true;
        config.object_store_endpoint = format!("127.0.0.1:{port}");
        config.object_store_bucket = bucket.to_string();
        config.object_store_access_key = "test-access".to_string();
        config.object_store_secret_key = "test-secret".to_string();
        config
    }

    fn finish_request_sends(store: &mut OwnedObjectStore) {
        let deadline = Instant::now() + Duration::from_secs(2);
        for slot in 0..store.async_read_slots() {
            while store.pending_read_wants_write(slot) {
                assert!(Instant::now() < deadline, "request send did not progress");
                crate::mem::guard::forbid_alloc(|| store.advance_pending_read(slot)).unwrap();
                std::thread::yield_now();
            }
        }
    }

    fn read_request(stream: &mut std::net::TcpStream) {
        stream.set_nonblocking(true).unwrap();
        let mut request = [0u8; 1024];
        loop {
            match stream.read(&mut request) {
                Ok(0) => panic!("client closed before sending its request"),
                Ok(_) => return,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::yield_now()
                }
                Err(error) => panic!("read request: {error}"),
            }
        }
    }

    #[test]
    fn parallel_reads_shared_results_survive_other_demand_until_every_consumer_finishes() {
        for packed in [false, true] {
            for missing in [false, true] {
                let listener = TcpListener::bind("127.0.0.1:0").unwrap();
                let mut config = network_config(
                    listener.local_addr().unwrap().port(),
                    "shared-read-consumers",
                );
                config.object_store_get_slots = 1;
                let mut budget = Budget::new(OwnedObjectStore::budget_bytes(&config));
                let mut store = OwnedObjectStore::new(&config, &mut budget, "blocks/").unwrap();
                store.enable_async_gets();
                let payload = b"one immutable response for two suspended queries";
                let (id, frame) = atomic_read_frame(payload);
                let container = BlockId::of(b"shared packed container");
                let mut output = [0u8; 128];
                let get = |store: &mut OwnedObjectStore, output: &mut [u8]| {
                    if packed {
                        store.get_packed(&container, 0, frame.len(), &id, output, &mut [])
                    } else {
                        store.get(&id, output)
                    }
                };
                crate::mem::guard::forbid_alloc(|| {
                    for owner in [ReadOwner(1), ReadOwner(2)] {
                        store.set_read_owner(Some(owner));
                        assert_eq!(get(&mut store, &mut output), Err(StoreError::NotReady));
                    }
                    assert_eq!(store.io_stats().object_gets, 1);
                });
                finish_request_sends(&mut store);
                let (mut stream, _) = atomic_read_request(&listener);
                atomic_read_response(&mut stream, &frame, packed, missing);
                drop(stream);
                let deadline = Instant::now() + Duration::from_secs(2);
                while !crate::mem::guard::forbid_alloc(|| store.advance_pending_read(0)).unwrap() {
                    assert!(Instant::now() < deadline, "shared response did not arrive");
                    std::thread::yield_now();
                }
                let expected = if missing {
                    Err(StoreError::NotFound)
                } else {
                    Ok((payload.len(), BlockType::SstData))
                };
                crate::mem::guard::forbid_alloc(|| {
                    store.set_read_owner(Some(ReadOwner(1)));
                    assert_eq!(get(&mut store, &mut output), expected);
                    if !missing {
                        assert_eq!(&output[..payload.len()], payload);
                    }
                    assert!(!OwnedObjectStore::slot_is_free(&store.slots[0]));
                    store.set_read_owner(Some(ReadOwner(3)));
                    assert_eq!(
                        store.get(&BlockId::of(b"unrelated demand"), &mut output),
                        Err(StoreError::NotReady)
                    );
                    assert_eq!(store.io_stats().object_gets, 1);
                    store.set_read_owner(Some(ReadOwner(2)));
                    assert_eq!(get(&mut store, &mut output), expected);
                    if !missing {
                        assert_eq!(&output[..payload.len()], payload);
                    }
                    assert!(OwnedObjectStore::slot_is_free(&store.slots[0]));
                    assert_eq!(store.read_capacity_generation(), 1);
                });
            }
        }
    }

    #[test]
    fn parallel_reads_cancel_only_their_consumer_and_release_hedged_requests() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut config =
            network_config(listener.local_addr().unwrap().port(), "cancel-shared-hedge");
        config.object_store_get_slots = 2;
        config.object_store_hedge_after_ms = 1;
        let mut budget = Budget::new(OwnedObjectStore::budget_bytes(&config));
        let mut store = OwnedObjectStore::new(&config, &mut budget, "blocks/").unwrap();
        store.enable_async_gets();
        let id = BlockId::of(b"held shared and hedged response");
        let mut output = [0u8; 64];
        crate::mem::guard::forbid_alloc(|| {
            for owner in [ReadOwner(1), ReadOwner(2)] {
                store.set_read_owner(Some(owner));
                assert_eq!(store.get(&id, &mut output), Err(StoreError::NotReady));
            }
            let started = store.slots[0].started_at.unwrap();
            assert!(!store.advance_read_deadlines(started + Duration::from_millis(1)));
            for slot in store.slots.iter() {
                assert_eq!(slot.pending_id, Some(id));
                assert_eq!(slot.owners.len(), 2);
            }
            store.cancel_read_owner(ReadOwner(1));
            for slot in store.slots.iter() {
                assert_eq!(slot.owners.as_slice(), &[ReadOwner(2)]);
                assert!(slot.client.pending_fd().is_some());
            }
            assert_eq!(store.read_capacity_generation(), 0);
            store.cancel_read_owner(ReadOwner(2));
            for slot in store.slots.iter() {
                assert!(OwnedObjectStore::slot_is_free(slot));
                assert!(slot.client.pending_fd().is_none());
            }
            assert_eq!(store.read_capacity_generation(), 2);
            assert!(store.next_read_deadline().is_none());
        });
    }

    #[test]
    fn parallel_reads_require_owners_and_reject_consumer_exhaustion_without_losing_requests() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut config = network_config(
            listener.local_addr().unwrap().port(),
            "read-consumer-capacity",
        );
        config.object_store_get_slots = 1;
        config.max_connections = 0;
        config.max_subscriptions = 0;
        let mut budget = Budget::new(OwnedObjectStore::budget_bytes(&config));
        let mut store = OwnedObjectStore::new(&config, &mut budget, "blocks/").unwrap();
        store.enable_async_gets();
        let id = BlockId::of(b"one reserved consumer");
        let mut output = [0u8; 64];
        crate::mem::guard::forbid_alloc(|| {
            assert_eq!(
                store.get(&id, &mut output),
                Err(StoreError::ReadOwnerRequired)
            );
            assert_eq!(store.io_stats().object_gets, 0);
            store.set_read_owner(Some(ReadOwner(1)));
            assert_eq!(store.get(&id, &mut output), Err(StoreError::NotReady));
            let fd = store.pending_read_fd(0).unwrap();
            store.set_read_owner(Some(ReadOwner(2)));
            assert_eq!(
                store.get(&id, &mut output),
                Err(StoreError::ReadOwnersExhausted)
            );
            assert_eq!(store.pending_read_fd(0), Some(fd));
            assert_eq!(store.slots[0].owners.as_slice(), &[ReadOwner(1)]);
            assert_eq!(store.io_stats().object_gets, 1);
            store.cancel_read_owner(ReadOwner(1));
            assert!(OwnedObjectStore::slot_is_free(&store.slots[0]));
        });
    }

    #[test]
    fn parallel_reads_cache_hits_release_shared_responses_before_further_demand() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut config = network_config(
            listener.local_addr().unwrap().port(),
            "shared-cache-consumers",
        );
        config.object_store_get_slots = 1;
        let mut budget = Budget::new(OwnedObjectStore::budget_bytes(&config) + (1 << 20));
        let store = OwnedObjectStore::new(&config, &mut budget, "blocks/").unwrap();
        let mut cache =
            super::super::BlockCache::new(&mut budget, "shared response cache", store, 1).unwrap();
        cache.enable_async_gets();
        let payload = b"the second query can use the first query's cached response";
        let (id, frame) = atomic_read_frame(payload);
        let mut output = [0u8; 128];
        crate::mem::guard::forbid_alloc(|| {
            for owner in [ReadOwner(1), ReadOwner(2)] {
                cache.set_read_owner(Some(owner));
                assert_eq!(cache.get(&id, &mut output), Err(StoreError::NotReady));
            }
        });
        finish_request_sends(cache.inner_mut());
        let (mut stream, _) = atomic_read_request(&listener);
        atomic_read_response(&mut stream, &frame, false, false);
        drop(stream);
        let deadline = Instant::now() + Duration::from_secs(2);
        while !crate::mem::guard::forbid_alloc(|| cache.advance_pending_read(0)).unwrap() {
            assert!(
                Instant::now() < deadline,
                "shared cache response did not arrive"
            );
            std::thread::yield_now();
        }
        crate::mem::guard::forbid_alloc(|| {
            for owner in [ReadOwner(1), ReadOwner(2)] {
                cache.set_read_owner(Some(owner));
                assert_eq!(
                    cache.get(&id, &mut output).unwrap(),
                    (payload.len(), BlockType::SstData)
                );
                assert_eq!(&output[..payload.len()], payload);
            }
            assert_eq!(cache.io_stats().ram_hits, 1);
            assert_eq!(cache.io_stats().object_gets, 1);
            assert!(OwnedObjectStore::slot_is_free(&cache.inner_mut().slots[0]));
            assert_eq!(cache.read_capacity_generation(), 1);
            assert_eq!(
                cache.get(&BlockId::of(b"next uncached block"), &mut output),
                Err(StoreError::NotReady)
            );
            assert_eq!(cache.io_stats().object_gets, 2);
            cache.cancel_read_owner(ReadOwner(2));
            assert!(OwnedObjectStore::slot_is_free(&cache.inner_mut().slots[0]));
        });
    }

    #[test]
    fn parallel_reads_timeout_whole_and_packed_requests_without_waiting_for_socket_events() {
        use std::net::TcpListener;
        for packed in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let config = network_config(listener.local_addr().unwrap().port(), "read-timeout");
            let mut budget = Budget::new(OwnedObjectStore::budget_bytes(&config));
            let mut store = OwnedObjectStore::new(&config, &mut budget, "blocks/").unwrap();
            store.enable_async_gets();
            store.set_read_owner(Some(ReadOwner(1)));
            let id = BlockId::of(b"held read");
            let container = BlockId::of(b"held packed read");
            let mut output = [0u8; 128];
            crate::mem::guard::forbid_alloc(|| {
                let initial = if packed {
                    store.get_packed(&container, 0, 80, &id, &mut output, &mut [])
                } else {
                    store.get(&id, &mut output)
                };
                assert_eq!(initial, Err(StoreError::NotReady));
                let started = store.slots[0].started_at.unwrap();
                assert_eq!(
                    store.next_read_deadline(),
                    Some(started + Duration::from_secs(30))
                );
                assert!(!store.advance_read_deadlines(started + Duration::from_secs(29)));
                assert!(store.advance_read_deadlines(started + Duration::from_secs(30)));
                assert!(store.pending_read_fd(0).is_none());
                assert!(store.next_read_deadline().is_none());
                let result = if packed {
                    store.get_packed(&container, 0, 80, &id, &mut output, &mut [])
                } else {
                    store.get(&id, &mut output)
                };
                assert_eq!(result, Err(StoreError::ReadTimedOut));
                assert!(OwnedObjectStore::slot_is_free(&store.slots[0]));
            });
        }
    }

    #[test]
    fn cleanup_download_overlap_request_pool_charges_clients_controls_and_scratch() {
        let mut config = network_config(9, "request-budget");
        config.object_store_head_bytes = 4096;
        config.object_store_response_bytes = super::super::BLOCK_SIZE;
        for slots in [1, 2, 7] {
            config.object_store_get_slots = slots;
            let mut budget = Budget::new(OwnedObjectStore::budget_bytes(&config));
            let store = OwnedObjectStore::new(&config, &mut budget, "blocks/").unwrap();
            assert_eq!(budget.remaining(), 0);
            assert_eq!(store.slots.capacity(), slots);
            assert_eq!(store.scratch.len(), super::super::BLOCK_SIZE);
        }
    }

    #[test]
    fn cleanup_download_overlap_short_request_budget_fails_before_runtime() {
        let mut config = network_config(9, "request-budget-short");
        config.object_store_head_bytes = 4096;
        config.object_store_response_bytes = super::super::BLOCK_SIZE;
        for slots in [1, 2, 7] {
            config.object_store_get_slots = slots;
            let mut budget = Budget::new(OwnedObjectStore::budget_bytes(&config) - 1);
            let error = OwnedObjectStore::new(&config, &mut budget, "blocks/")
                .err()
                .expect("one missing scratch byte must reject startup");
            let crate::object_store::SetupError::Budget(error) = error else {
                panic!("short request budget must return its named budget error");
            };
            assert_eq!(error.what, "object_store_block_scratch");
        }
    }

    fn atomic_read_frame(payload: &[u8]) -> (BlockId, Vec<u8>) {
        let mut bytes = vec![0u8; super::super::BLOCK_SIZE];
        let (id, length) = encode(payload, BlockType::SstData, 0, &mut bytes).unwrap();
        bytes.truncate(length);
        (id, bytes)
    }

    fn atomic_read_request(listener: &TcpListener) -> (std::net::TcpStream, Vec<u8>) {
        listener.set_nonblocking(true).unwrap();
        let deadline = Instant::now() + std::time::Duration::from_secs(2);
        let (mut stream, _) = loop {
            match listener.accept() {
                Ok(connection) => break connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        Instant::now() < deadline,
                        "expected block request did not arrive"
                    );
                    std::thread::yield_now();
                }
                Err(error) => panic!("accept block request: {error}"),
            }
        };
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        stream
            .set_write_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        let mut request = Vec::new();
        let mut bytes = [0u8; 2048];
        while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            let length = stream.read(&mut bytes).unwrap();
            assert!(length > 0 && request.len() + length <= 4096);
            request.extend_from_slice(&bytes[..length]);
        }
        assert!(request.starts_with(b"GET "));
        (stream, request)
    }

    fn atomic_read_response(
        stream: &mut std::net::TcpStream,
        bytes: &[u8],
        packed: bool,
        missing: bool,
    ) {
        if missing {
            stream
                .write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .unwrap();
            return;
        }
        let status = if packed {
            "206 Partial Content"
        } else {
            "200 OK"
        };
        write!(stream, "HTTP/1.1 {status}\r\nETag: \"atomic-read\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", bytes.len()).unwrap();
        stream.write_all(bytes).unwrap();
    }

    #[derive(Clone, Copy)]
    enum AtomicReply {
        Data,
        MissingWhole,
        CorruptWhole,
        CorruptPacked,
    }

    fn check_atomic_reads_preserve_pending(
        packed_pending: bool,
        missing_pending: bool,
        atomic_reply: AtomicReply,
        same_atomic: bool,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let pending_payload = b"this response still belongs to the suspended reader";
        let atomic_payload: &[u8] = if same_atomic {
            pending_payload
        } else {
            b"the writer needs this different block"
        };
        let packed_payload = b"the writer also needs this packed extent";
        let (pending_id, pending_frame) = atomic_read_frame(pending_payload);
        let (atomic_id, atomic_frame) = atomic_read_frame(atomic_payload);
        let (packed_id, packed_frame) = atomic_read_frame(packed_payload);
        let pending_length = pending_frame.len();
        let atomic_length = atomic_frame.len();
        let packed_length = packed_frame.len();
        let (started, request_started) = mpsc::channel();
        let (release, response_released) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut pending, request) = atomic_read_request(&listener);
            assert_eq!(
                request.windows(6).any(|bytes| bytes == b"range:"),
                packed_pending
            );
            started.send(()).unwrap();
            let (mut atomic, _) = atomic_read_request(&listener);
            if matches!(atomic_reply, AtomicReply::CorruptWhole) {
                let mut damaged = atomic_frame.clone();
                *damaged.last_mut().unwrap() ^= 0x80;
                atomic_read_response(&mut atomic, &damaged, false, false);
            } else {
                atomic_read_response(
                    &mut atomic,
                    &atomic_frame,
                    false,
                    matches!(atomic_reply, AtomicReply::MissingWhole),
                );
            }
            drop(atomic);
            let (mut packed, request) = atomic_read_request(&listener);
            assert!(request.windows(6).any(|bytes| bytes == b"range:"));
            if matches!(atomic_reply, AtomicReply::CorruptPacked) {
                let mut damaged = packed_frame.clone();
                *damaged.last_mut().unwrap() ^= 0x80;
                atomic_read_response(&mut packed, &damaged, true, false);
            } else {
                atomic_read_response(&mut packed, &packed_frame, true, false);
            }
            drop(packed);
            response_released
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap();
            atomic_read_response(
                &mut pending,
                &pending_frame,
                packed_pending,
                missing_pending,
            );
            drop(pending);
            let (mut reused, _) = atomic_read_request(&listener);
            atomic_read_response(&mut reused, &atomic_frame, false, false);
        });
        let mut config = network_config(port, "atomic-read-ownership");
        config.object_store_get_slots = 1;
        let mut budget = Budget::new(OwnedObjectStore::budget_bytes(&config));
        let mut store = OwnedObjectStore::new(&config, &mut budget, "blocks/").unwrap();
        store.enable_async_gets();
        store.set_read_owner(Some(ReadOwner(1)));
        let pending_container = BlockId::of(b"pending packed container");
        let packed_container = BlockId::of(b"atomic packed container");
        let mut output = [0u8; 128];
        let initial = crate::mem::guard::forbid_alloc(|| {
            if packed_pending {
                store.get_packed(
                    &pending_container,
                    0,
                    pending_length,
                    &pending_id,
                    &mut output,
                    &mut [],
                )
            } else {
                store.get(&pending_id, &mut output)
            }
        });
        assert_eq!(initial, Err(StoreError::NotReady));
        finish_request_sends(&mut store);
        request_started
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        let fd = store.pending_read_fd(0).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            store.disable_async_gets();
            assert!(!store.async_gets_enabled());
            assert_eq!(store.pending_read_fd(0), Some(fd));
            let atomic = store.get(&atomic_id, &mut output);
            if matches!(atomic_reply, AtomicReply::MissingWhole) {
                assert_eq!(atomic, Err(StoreError::NotFound));
            } else if matches!(atomic_reply, AtomicReply::CorruptWhole) {
                assert_eq!(
                    atomic,
                    Err(StoreError::Corrupt(
                        super::super::BlockError::ChecksumMismatch
                    ))
                );
            } else {
                assert_eq!(atomic.unwrap(), (atomic_payload.len(), BlockType::SstData));
                assert_eq!(&output[..atomic_payload.len()], atomic_payload);
            }
            assert_eq!(store.pending_read_fd(0), Some(fd));
            let packed = store.get_packed(
                &packed_container,
                0,
                packed_length,
                &packed_id,
                &mut output,
                &mut [],
            );
            if matches!(atomic_reply, AtomicReply::CorruptPacked) {
                assert_eq!(
                    packed,
                    Err(StoreError::Corrupt(
                        super::super::BlockError::ChecksumMismatch
                    ))
                );
            } else {
                assert_eq!(packed.unwrap(), (packed_payload.len(), BlockType::SstData));
                assert_eq!(&output[..packed_payload.len()], packed_payload);
            }
            assert_eq!(store.pending_read_fd(0), Some(fd));
            assert!(store.async_reads_busy());
            store.enable_async_gets();
            store.set_read_owner(Some(ReadOwner(1)));
        });
        release.send(()).unwrap();
        let deadline = Instant::now() + std::time::Duration::from_secs(2);
        crate::mem::guard::forbid_alloc(|| {
            while !store.advance_pending_read(0).unwrap() {
                assert!(
                    Instant::now() < deadline,
                    "released response did not complete"
                );
                std::thread::yield_now();
            }
            store.disable_async_gets();
            let pending = if packed_pending {
                store.get_packed(
                    &pending_container,
                    0,
                    pending_length,
                    &pending_id,
                    &mut output,
                    &mut [],
                )
            } else {
                store.get(&pending_id, &mut output)
            };
            if missing_pending {
                assert_eq!(pending, Err(StoreError::NotFound));
            } else {
                assert_eq!(
                    pending.unwrap(),
                    (pending_payload.len(), BlockType::SstData)
                );
                assert_eq!(&output[..pending_payload.len()], pending_payload);
            }
            assert!(!store.async_reads_busy());
            assert_eq!(store.io_stats().object_gets, 3);
            let expected_bytes = packed_length
                + if matches!(atomic_reply, AtomicReply::MissingWhole) {
                    0
                } else {
                    atomic_length
                }
                + if missing_pending { 0 } else { pending_length };
            assert_eq!(store.io_stats().object_read_bytes, expected_bytes as u64);
            store.enable_async_gets();
            store.set_read_owner(Some(ReadOwner(1)));
            let mut result = store.get(&atomic_id, &mut output);
            while result == Err(StoreError::NotReady) {
                assert!(
                    Instant::now() < deadline,
                    "released slot could not serve the next read"
                );
                if store.advance_pending_read(0).unwrap() {
                    result = store.get(&atomic_id, &mut output);
                }
                std::thread::yield_now();
            }
            assert_eq!(result.unwrap(), (atomic_payload.len(), BlockType::SstData));
            assert_eq!(&output[..atomic_payload.len()], atomic_payload);
            assert_eq!(store.io_stats().object_gets, 4);
            assert_eq!(
                store.io_stats().object_read_bytes,
                (expected_bytes + atomic_length) as u64
            );
        });
        server.join().unwrap();
    }

    #[test]
    fn atomic_block_reads_whole_misses_preserve_a_pending_whole_response() {
        check_atomic_reads_preserve_pending(false, false, AtomicReply::Data, false);
    }

    #[test]
    fn atomic_block_reads_whole_and_packed_misses_preserve_a_pending_packed_response() {
        check_atomic_reads_preserve_pending(true, false, AtomicReply::Data, false);
    }

    #[test]
    fn atomic_block_reads_preserve_a_pending_error_and_reuse_the_released_slot() {
        check_atomic_reads_preserve_pending(false, true, AtomicReply::Data, false);
    }

    #[test]
    fn atomic_block_reads_atomic_error_preserves_the_unrelated_pending_response() {
        check_atomic_reads_preserve_pending(true, false, AtomicReply::MissingWhole, false);
    }

    #[test]
    fn atomic_block_reads_same_block_miss_never_borrows_the_pending_read_slot() {
        for packed in [false, true] {
            check_atomic_reads_preserve_pending(packed, false, AtomicReply::Data, true);
        }
    }

    #[test]
    fn atomic_block_reads_corrupt_whole_response_keeps_bytes_and_pending_owner() {
        check_atomic_reads_preserve_pending(false, false, AtomicReply::CorruptWhole, false);
    }

    #[test]
    fn atomic_block_reads_corrupt_packed_response_keeps_bytes_and_pending_owner() {
        check_atomic_reads_preserve_pending(true, false, AtomicReply::CorruptPacked, false);
    }

    #[test]
    fn atomic_block_reads_invalid_packed_ranges_do_not_touch_owned_requests() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut config = network_config(
            listener.local_addr().unwrap().port(),
            "atomic-invalid-range",
        );
        config.object_store_get_slots = 1;
        let mut budget = Budget::new(OwnedObjectStore::budget_bytes(&config));
        let mut store = OwnedObjectStore::new(&config, &mut budget, "blocks/").unwrap();
        store.enable_async_gets();
        store.set_read_owner(Some(ReadOwner(1)));
        let id = BlockId::of(b"owned pending request");
        let mut output = [0u8; 32];
        assert_eq!(
            crate::mem::guard::forbid_alloc(|| store.get(&id, &mut output)),
            Err(StoreError::NotReady)
        );
        finish_request_sends(&mut store);
        let (accepted, _) = atomic_read_request(&listener);
        let fd = store.pending_read_fd(0).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            for restartable in [true, false] {
                if restartable {
                    store.enable_async_gets();
                    store.set_read_owner(Some(ReadOwner(1)));
                } else {
                    store.disable_async_gets();
                }
                for (offset, length) in [(usize::MAX, 1), (0, 0), (0, usize::MAX)] {
                    assert_eq!(
                        store.get_packed(&id, offset, length, &id, &mut output, &mut []),
                        Err(StoreError::Corrupt(super::super::BlockError::Truncated))
                    );
                    assert_eq!(store.pending_read_fd(0), Some(fd));
                    assert!(store.async_reads_busy());
                    assert_eq!(store.io_stats().object_gets, 1);
                }
            }
            assert_eq!(
                listener.accept().unwrap_err().kind(),
                std::io::ErrorKind::WouldBlock
            );
        });
        drop(accepted);
    }

    #[test]
    fn async_pool_owns_two_independent_block_requests() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (release, proceed) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut first, _) = listener.accept().unwrap();
            read_request(&mut first);
            let (mut second, _) = listener.accept().unwrap();
            read_request(&mut second);
            proceed.recv().unwrap();
        });

        let mut config = network_config(port, "pool-test");
        config.object_store_get_slots = 2;
        let mut budget = Budget::new(16 << 20);
        let mut store = OwnedObjectStore::new(&config, &mut budget, "blocks/").unwrap();
        store.enable_async_gets();
        store.set_read_owner(Some(ReadOwner(1)));

        let first = BlockId::of(b"first");
        let second = BlockId::of(b"second");
        let mut output = [0u8; 32];
        assert_eq!(store.get(&first, &mut output), Err(StoreError::NotReady));
        assert_eq!(store.get(&second, &mut output), Err(StoreError::NotReady));
        let first_fd = store.pending_read_fd(0).expect("first slot pending");
        let second_fd = store.pending_read_fd(1).expect("second slot pending");
        assert_ne!(first_fd, second_fd);
        assert!(store.async_reads_busy());
        finish_request_sends(&mut store);
        drop(store);
        release.send(()).unwrap();
        server.join().unwrap();
    }

    #[test]
    fn prefetch_keeps_a_completed_body_for_its_demand_read() {
        let payload = b"prefetched body";
        let mut framed = [0u8; super::super::BLOCK_SIZE];
        let (id, framed_len) = encode(payload, BlockType::SstData, 0, &mut framed).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let body = framed[..framed_len].to_vec();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 1024];
            let request_len = stream.read(&mut request).unwrap();
            assert!(request_len > 0, "client sent an empty request");
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nETag: \"prefetch-read\"\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n",
                body.len()
            )
            .unwrap();
            stream.write_all(&body).unwrap();
            listener.set_nonblocking(true).unwrap();
            stream.set_nonblocking(true).unwrap();
            for _ in 0..1_000_000 {
                match listener.accept() {
                    Ok((mut writer, _)) => {
                        read_request(&mut writer);
                        writer
                            .write_all(b"HTTP/1.1 200 OK\r\nETag: \"prefetch-write\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                            .unwrap();
                        return;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(error) => panic!("accept writer: {error}"),
                }
                let mut request = [0u8; 1];
                match stream.read(&mut request) {
                    Ok(count) if count > 0 => panic!("write reused a completed read client"),
                    Ok(_) => panic!("read client closed before writer connected"),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(error) => panic!("read completed client: {error}"),
                }
                std::thread::yield_now();
            }
            panic!("writer did not use its dedicated object client");
        });

        let mut config = network_config(port, "prefetch-test");
        config.object_store_get_slots = 1;
        let mut budget = Budget::new(16 << 20);
        let mut store = OwnedObjectStore::new(&config, &mut budget, "blocks/").unwrap();
        store.enable_async_gets();
        store.set_read_owner(Some(ReadOwner(1)));
        assert_eq!(store.prefetch(&id).unwrap(), PrefetchState::Scheduled);
        let mut complete = false;
        for _ in 0..10_000 {
            if store.advance_pending_read(0).unwrap() {
                complete = true;
                break;
            }
            std::thread::yield_now();
        }
        assert!(complete, "mock object response did not complete");
        store
            .put(b"concurrent write", BlockType::SstIndexV2, 0)
            .unwrap();
        let mut output = [0u8; 64];
        assert_eq!(
            store.take_prefetch(&id, &mut output).unwrap(),
            Some((payload.len(), BlockType::SstData))
        );
        assert_eq!(&output[..payload.len()], payload);
        let stats = store.io_stats();
        assert_eq!(stats.object_prefetch_scheduled, 1);
        assert_eq!(stats.object_prefetch_reused, 0);
        assert_eq!(stats.object_prefetch_saturated, 0);
        assert_eq!(stats.object_read_completions, 1);
        assert_eq!(stats.object_read_bytes, framed_len as u64);
        server.join().unwrap();
    }

    #[test]
    fn abandoned_prefetch_does_not_starve_demand_or_checkpoint() {
        let mut config = crate::config::Config::default_dev();
        config.object_store_on = true;
        config.object_store_sim = true;
        config.object_store_get_slots = 1;
        config.object_store_bucket = format!("abandoned-prefetch-{}", std::process::id());
        crate::object_store::sim::drop_namespace(&config.object_store_bucket);
        let mut budget = Budget::new(16 << 20);
        let mut store = OwnedObjectStore::new(&config, &mut budget, "blocks/").unwrap();
        let first = store.put(b"first", BlockType::SstData, 0).unwrap();
        let second = store.put(b"second", BlockType::SstData, 0).unwrap();
        store.enable_async_gets();
        store.set_read_owner(Some(ReadOwner(1)));

        assert_eq!(store.prefetch(&first).unwrap(), PrefetchState::Scheduled);
        assert!(!store.async_reads_busy());
        let mut output = [0; 32];
        assert_eq!(
            store.get(&second, &mut output).unwrap(),
            (6, BlockType::SstData)
        );
        assert_eq!(&output[..6], b"second");
        assert_eq!(
            store.take_prefetch(&first, &mut output).unwrap(),
            Some((5, BlockType::SstData))
        );
        assert_eq!(&output[..5], b"first");

        assert_eq!(store.prefetch(&second).unwrap(), PrefetchState::Scheduled);
        store.slots[0].speculative = false;
        assert_eq!(store.get(&first, &mut output), Err(StoreError::NotReady));
        assert_eq!(
            store.take_prefetch(&second, &mut output).unwrap(),
            Some((6, BlockType::SstData))
        );
        assert_eq!(&output[..6], b"second");
        assert_eq!(
            store.get(&first, &mut output).unwrap(),
            (5, BlockType::SstData)
        );
        assert_eq!(&output[..5], b"first");

        assert_eq!(store.prefetch(&second).unwrap(), PrefetchState::Scheduled);
        assert!(!store.async_reads_busy());
        store.disable_async_gets();
        assert_eq!(store.slots[0].ready_id, Some(second));
        assert_eq!(
            store.take_prefetch(&second, &mut output).unwrap(),
            Some((6, BlockType::SstData))
        );
        assert!(store.slots[0].ready_id.is_none());
        drop(store);
        crate::object_store::sim::drop_namespace(&config.object_store_bucket);
    }

    #[test]
    fn packed_read_remains_reactor_owned_until_its_extent_is_consumed() {
        let payload = b"packed range body";
        let mut framed = [0u8; super::super::BLOCK_SIZE];
        let (expected, framed_len) = encode(payload, BlockType::SstData, 0, &mut framed).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let body = framed[..framed_len].to_vec();
        let (request_started, request_started_rx) = mpsc::channel();
        let (release_response_tx, release_response) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 1024];
            let request_len = stream.read(&mut request).unwrap();
            let request = std::str::from_utf8(&request[..request_len]).unwrap();
            assert!(request.contains(&format!("range: bytes={HEADER_LEN}-")));
            request_started.send(()).unwrap();
            release_response.recv().unwrap();
            write!(
                stream,
                "HTTP/1.1 206 Partial Content\r\nETag: \"packed-read\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            stream.write_all(&body).unwrap();
        });

        let mut config = network_config(port, "packed-reactor-test");
        config.object_store_get_slots = 1;
        let mut budget = Budget::new(16 << 20);
        let mut store = OwnedObjectStore::new(&config, &mut budget, "blocks/").unwrap();
        store.enable_async_gets();
        store.set_read_owner(Some(ReadOwner(1)));

        let container = BlockId::of(b"packed container");
        let mut output = [0u8; 64];
        assert_eq!(
            store.get_packed(&container, 0, framed_len, &expected, &mut output, &mut []),
            Err(StoreError::NotReady)
        );
        finish_request_sends(&mut store);
        request_started_rx.recv().unwrap();
        assert!(store.async_reads_busy());
        assert!(store.next_read_deadline().is_some());
        release_response_tx.send(()).unwrap();

        let mut complete = false;
        for _ in 0..10_000 {
            if store.advance_pending_read(0).unwrap() {
                complete = true;
                break;
            }
            std::thread::yield_now();
        }
        assert!(complete, "mock packed response did not complete");
        assert!(!store.async_reads_busy());
        store.disable_async_gets();
        assert_eq!(
            store
                .get_packed(&container, 0, framed_len, &expected, &mut output, &mut [])
                .unwrap(),
            (payload.len(), BlockType::SstData)
        );
        assert_eq!(&output[..payload.len()], payload);
        assert!(!store.async_reads_busy());
        server.join().unwrap();
    }

    #[test]
    fn advancing_an_inline_completed_prefetch_reports_completion() {
        let mut config = crate::config::Config::default_dev();
        config.object_store_on = true;
        config.object_store_sim = true;
        config.object_store_get_slots = 1;
        let mut budget = Budget::new(16 << 20);
        let mut store = OwnedObjectStore::new(&config, &mut budget, "blocks/").unwrap();
        let id = BlockId::of(b"inline completion");

        store.slots[0].ready_id = Some(id);
        assert!(store.advance_pending_read(0).unwrap());
    }

    #[test]
    fn a_released_slot_reports_a_stale_readiness_event() {
        let mut config = crate::config::Config::default_dev();
        config.object_store_on = true;
        config.object_store_sim = true;
        config.object_store_get_slots = 1;
        let mut budget = Budget::new(16 << 20);
        let mut store = OwnedObjectStore::new(&config, &mut budget, "blocks/").unwrap();

        assert!(!store.advance_pending_read(0).unwrap());
        assert_eq!(store.io_stats().object_read_stale_events, 1);
    }

    #[test]
    fn packed_read_uses_only_the_requested_verified_extent() {
        let mut config = crate::config::Config::default_dev();
        config.object_store_on = true;
        config.object_store_sim = true;
        config.object_store_get_slots = 1;
        let mut budget = Budget::new(16 << 20);
        let mut store = OwnedObjectStore::new(&config, &mut budget, "blocks/").unwrap();

        let mut first = [0u8; super::super::BLOCK_SIZE];
        let (first_id, first_len) =
            encode(b"first", BlockType::SstDataPaxColumnV1, 0, &mut first).unwrap();
        let mut second = [0u8; super::super::BLOCK_SIZE];
        let (second_id, second_len) =
            encode(b"second", BlockType::SstDataPaxColumnV1, 0, &mut second).unwrap();
        let mut container = vec![0u8; first_len + second_len];
        container[..first_len].copy_from_slice(&first[..first_len]);
        container[first_len..].copy_from_slice(&second[..second_len]);
        let container_id = store
            .put(&container, BlockType::SstPackedContainerV1, 0)
            .unwrap();

        let before = store.io_stats();
        let mut output = [0u8; 32];
        let mut scratch = [0u8; 32];
        assert_eq!(
            store
                .get_packed(
                    &container_id,
                    first_len,
                    second_len,
                    &second_id,
                    &mut output,
                    &mut scratch,
                )
                .unwrap(),
            (6, BlockType::SstDataPaxColumnV1)
        );
        assert_eq!(&output[..6], b"second");
        let after = store.io_stats().saturating_sub(before);
        assert_eq!(after.object_gets, 1);
        assert_eq!(after.object_read_bytes, second_len as u64);

        assert!(
            store
                .get_packed(
                    &container_id,
                    0,
                    first_len,
                    &second_id,
                    &mut output,
                    &mut scratch,
                )
                .is_err()
        );
        assert_ne!(first_id, second_id);
    }

    #[test]
    fn due_hedge_uses_a_spare_slot_and_releases_the_stalled_request() {
        let payload = b"hedged body";
        let mut framed = [0u8; super::super::BLOCK_SIZE];
        let (id, framed_len) = encode(payload, BlockType::SstData, 0, &mut framed).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let body = framed[..framed_len].to_vec();
        let (hedge_arrived, hedge_ready) = mpsc::channel();
        let (release_hedge, write_response) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut stalled, _) = listener.accept().unwrap();
            let mut request = [0u8; 1024];
            assert!(stalled.read(&mut request).unwrap() > 0);
            let (mut hedge, _) = listener.accept().unwrap();
            assert!(hedge.read(&mut request).unwrap() > 0);
            hedge_arrived.send(()).unwrap();
            write_response.recv().unwrap();
            write!(
                hedge,
                "HTTP/1.1 200 OK\r\nETag: \"hedge-read\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            hedge.write_all(&body).unwrap();
            // Keep the original socket alive until the winner consumes the
            // duplicate, proving winner selection actively cancels it.
            let mut byte = [0u8; 1];
            let _ = stalled.read(&mut byte);
        });

        let mut config = network_config(port, "hedge-test");
        config.object_store_get_slots = 2;
        config.object_store_hedge_after_ms = 1;
        let mut budget = Budget::new(16 << 20);
        let mut store = OwnedObjectStore::new(&config, &mut budget, "blocks/").unwrap();
        store.enable_async_gets();
        store.set_read_owner(Some(ReadOwner(1)));

        let mut output = [0u8; 64];
        assert_eq!(store.get(&id, &mut output), Err(StoreError::NotReady));
        let deadline = store
            .next_read_deadline()
            .expect("pending read has a deadline");
        // Exercise the timer's strictly-due branch, not equality at an
        // `Instant` boundary whose precision differs across CI runners.
        store.advance_read_deadlines(deadline + std::time::Duration::from_nanos(1));
        finish_request_sends(&mut store);
        hedge_ready.recv().unwrap();
        assert!(
            store.pending_read_fd(1).is_some(),
            "hedge owns the spare slot"
        );
        assert_eq!(store.io_stats().object_gets, 2);
        release_hedge.send(()).unwrap();
        let mut complete = false;
        for _ in 0..10_000 {
            if store.advance_pending_read(1).unwrap() {
                complete = true;
                break;
            }
            std::thread::yield_now();
        }
        assert!(complete, "hedged response did not complete");
        assert_eq!(
            store.get(&id, &mut output).unwrap(),
            (payload.len(), BlockType::SstData)
        );
        assert_eq!(&output[..payload.len()], payload);
        assert!(
            store.pending_read_fd(0).is_none(),
            "winner released the stalled sibling"
        );
        drop(store);
        server.join().unwrap();
    }
}
