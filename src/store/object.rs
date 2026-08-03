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

use crate::mem::budget::Budget;
use crate::mem::fixed_vec::FixedVec;
use crate::object_store::{Client as ObjectStore, Error as ObjectError, Precondition};
use std::time::{Duration, Instant};

use super::{
    BlockId, BlockIoStats, BlockStore, BlockType, HEADER_LEN, PrefetchState, StoreError, decode,
    encode,
};

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
    slots: FixedVec<ObjectReadSlot>,
    prefix: &'static str,
    scratch: Vec<u8>,
    stats: BlockIoStats,
    async_gets_enabled: bool,
    hedge_after: Option<Duration>,
}

struct ObjectReadSlot {
    client: ObjectStore,
    pending_id: Option<BlockId>,
    ready_id: Option<BlockId>,
    error_id: Option<BlockId>,
    pending_error: Option<StoreError>,
    started_at: Option<Instant>,
    hedge_issued: bool,
}

impl OwnedObjectStore {
    /// Startup-only: the scratch Vec is reserved once, before the allocator
    /// freezes, and never grows.
    pub(crate) fn new(
        config: &crate::config::Config,
        budget: &mut Budget,
        prefix: &'static str,
    ) -> Result<Self, crate::object_store::SetupError> {
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
                    pending_error: None,
                    started_at: None,
                    hedge_issued: false,
                })
                .expect("object read slots sized from configuration");
        }
        Ok(Self {
            slots,
            prefix,
            scratch: vec![0u8; super::BLOCK_SIZE],
            stats: BlockIoStats::default(),
            async_gets_enabled: false,
            hedge_after: (config.object_store_hedge_after_ms != 0)
                .then(|| Duration::from_millis(config.object_store_hedge_after_ms)),
        })
    }

    fn enable_async_gets(&mut self) {
        for slot in self.slots.as_mut_slice() {
            slot.client.enable_async_gets();
        }
        self.async_gets_enabled = true;
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
        for slot in self.slots.as_mut_slice() {
            slot.client.disable_async_gets();
        }
        self.async_gets_enabled = false;
    }

    fn pending_read_fd(&self, slot: usize) -> Option<std::os::fd::RawFd> {
        self.slots.get(slot)?.client.pending_get_fd()
    }

    fn advance_pending_read(&mut self, slot: usize) -> Result<bool, StoreError> {
        let (result, completed) = {
            let slot = self.slots.get_mut(slot).expect("reactor slot is bounded");
            if slot.pending_id.is_none() {
                assert!(
                    slot.ready_id.is_some() || slot.error_id.is_some(),
                    "advance called on a free object-read slot"
                );
                return Ok(true);
            }
            match slot.client.advance_get() {
                Ok(()) => {
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
                    slot.client.clear_pending_get();
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
    }

    fn release_siblings(&mut self, id: BlockId, winner: usize) {
        for (index, slot) in self.slots.as_mut_slice().iter_mut().enumerate() {
            if index == winner
                || (slot.pending_id != Some(id)
                    && slot.ready_id != Some(id)
                    && slot.error_id != Some(id))
            {
                continue;
            }
            if slot.pending_id.is_some() {
                slot.client.clear_pending_get();
            }
            slot.pending_id = None;
            slot.ready_id = None;
            slot.error_id = None;
            slot.pending_error = None;
            slot.started_at = None;
            slot.hedge_issued = false;
        }
    }

    fn next_hedge_deadline(&self) -> Option<Instant> {
        let hedge_after = self.hedge_after?;
        self.slots
            .iter()
            .filter(|slot| slot.pending_id.is_some() && !slot.hedge_issued)
            .filter_map(|slot| {
                slot.started_at
                    .and_then(|started| started.checked_add(hedge_after))
            })
            .min()
    }

    fn issue_due_hedges(&mut self, now: Instant) {
        let Some(deadline) = self.next_hedge_deadline() else {
            return;
        };
        if deadline > now {
            return;
        }
        let Some(source_index) = self.slots.iter().position(|slot| {
            slot.pending_id.is_some()
                && !slot.hedge_issued
                && slot
                    .started_at
                    .and_then(|started| {
                        self.hedge_after
                            .and_then(|after| started.checked_add(after))
                    })
                    .is_some_and(|due| due <= now)
        }) else {
            return;
        };
        let Some(destination_index) = self.slots.iter().position(Self::slot_is_free) else {
            return;
        };
        let id = self.slots[source_index]
            .pending_id
            .expect("selected pending hedge source");
        self.slots[source_index].hedge_issued = true;
        let mut key_buffer = [0u8; 128];
        let key = key_of(self.prefix, &id, &mut key_buffer);
        let completed = {
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
        if let Some(bytes) = completed {
            self.record_completed_read(now, bytes);
        }
        self.stats.object_gets = self.stats.object_gets.saturating_add(1);
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
            &mut self.slots[0].client,
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
        if let Some(winner) = self
            .slots
            .iter()
            .position(|slot| slot.ready_id == Some(*id))
        {
            self.release_siblings(*id, winner);
            let slot = &mut self.slots[winner];
            slot.ready_id = None;
            slot.hedge_issued = false;
            return decode_block_body(slot.client.body_bytes(), id, into);
        }
        if self.slots.iter().any(|slot| slot.pending_id == Some(*id)) {
            return Err(StoreError::NotReady);
        }
        if let Some(winner) = self
            .slots
            .iter()
            .position(|slot| slot.error_id == Some(*id))
        {
            self.release_siblings(*id, winner);
            let slot = &mut self.slots[winner];
            slot.error_id = None;
            slot.hedge_issued = false;
            return Err(slot.pending_error.take().expect("checked above"));
        }
        let started = Instant::now();
        let result = {
            let Some(slot) = self.slots.iter_mut().find(|slot| Self::slot_is_free(slot)) else {
                return Err(StoreError::NotReady);
            };
            let result = get_block(&mut slot.client, self.prefix, id, into);
            if matches!(result, Err(StoreError::NotReady)) {
                slot.pending_id = Some(*id);
                slot.started_at = Some(started);
            }
            result
        };
        if let Ok((len, _)) = result {
            self.record_completed_read(started, len);
        }
        self.stats.object_gets = self.stats.object_gets.saturating_add(1);
        result
    }

    fn prefetch(&mut self, id: &BlockId) -> Result<PrefetchState, StoreError> {
        for slot in self.slots.as_mut_slice() {
            if slot.pending_id == Some(*id)
                || slot.ready_id == Some(*id)
                || slot.error_id == Some(*id)
            {
                self.stats.object_prefetch_reused =
                    self.stats.object_prefetch_reused.saturating_add(1);
                return Ok(PrefetchState::Reused);
            }
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
            match slot.client.get(key, None) {
                Ok(result) => {
                    slot.ready_id = Some(*id);
                    Some(result.len)
                }
                Err(ObjectError::WouldBlock) => {
                    slot.pending_id = Some(*id);
                    slot.started_at = Some(started);
                    None
                }
                Err(error) => return Err(store_error(error)),
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
        if let Some(winner) = self
            .slots
            .iter()
            .position(|slot| slot.ready_id == Some(*id))
        {
            self.release_siblings(*id, winner);
            let slot = &mut self.slots[winner];
            slot.ready_id = None;
            slot.hedge_issued = false;
            return decode_block_body(slot.client.body_bytes(), id, into).map(Some);
        }
        if self.slots.iter().any(|slot| slot.pending_id == Some(*id)) {
            return Ok(None);
        }
        if let Some(winner) = self
            .slots
            .iter()
            .position(|slot| slot.error_id == Some(*id))
        {
            self.release_siblings(*id, winner);
            let slot = &mut self.slots[winner];
            slot.error_id = None;
            slot.hedge_issued = false;
            return Err(slot.pending_error.take().expect("checked above"));
        }
        Ok(None)
    }

    fn contains(&mut self, id: &BlockId) -> Result<bool, StoreError> {
        let mut key_buffer = [0u8; 128];
        let key = key_of(self.prefix, id, &mut key_buffer);
        self.stats.object_contains = self.stats.object_contains.saturating_add(1);
        match self.slots[0]
            .client
            .get(key, Some((0, HEADER_LEN as u64 - 1)))
        {
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

    fn async_gets_enabled(&self) -> bool {
        self.async_gets_enabled
    }

    fn async_read_slots(&self) -> usize {
        self.slots.len()
    }

    fn async_reads_busy(&self) -> bool {
        self.slots.iter().any(|slot| {
            slot.pending_id.is_some() || slot.ready_id.is_some() || slot.error_id.is_some()
        })
    }

    fn pending_read_fd(&self, slot: usize) -> Option<std::os::fd::RawFd> {
        self.pending_read_fd(slot)
    }

    fn advance_pending_read(&mut self, slot: usize) -> Result<bool, StoreError> {
        self.advance_pending_read(slot)
    }

    fn next_hedge_deadline(&self) -> Option<Instant> {
        self.next_hedge_deadline()
    }

    fn issue_due_hedges(&mut self, now: Instant) {
        self.issue_due_hedges(now);
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

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::io::Write;
    use std::net::TcpListener;
    use std::sync::mpsc;

    use super::*;

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

        let mut config = crate::config::Config::default_dev();
        config.object_store_on = true;
        config.object_store_endpoint = format!("127.0.0.1:{port}");
        config.object_store_bucket = "pool-test".to_string();
        config.object_store_access_key = "key".to_string();
        config.object_store_secret_key = "secret".to_string();
        config.object_store_get_slots = 2;
        let mut budget = Budget::new(16 << 20);
        let mut store = OwnedObjectStore::new(&config, &mut budget, "blocks/").unwrap();
        store.enable_async_gets();

        let first = BlockId::of(b"first");
        let second = BlockId::of(b"second");
        let mut output = [0u8; 32];
        assert_eq!(store.get(&first, &mut output), Err(StoreError::NotReady));
        assert_eq!(store.get(&second, &mut output), Err(StoreError::NotReady));
        let first_fd = store.pending_read_fd(0).expect("first slot pending");
        let second_fd = store.pending_read_fd(1).expect("second slot pending");
        assert_ne!(first_fd, second_fd);
        assert!(store.async_reads_busy());
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
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            stream.write_all(&body).unwrap();
        });

        let mut config = crate::config::Config::default_dev();
        config.object_store_on = true;
        config.object_store_endpoint = format!("127.0.0.1:{port}");
        config.object_store_bucket = "prefetch-test".to_string();
        config.object_store_access_key = "key".to_string();
        config.object_store_secret_key = "secret".to_string();
        config.object_store_get_slots = 1;
        let mut budget = Budget::new(16 << 20);
        let mut store = OwnedObjectStore::new(&config, &mut budget, "blocks/").unwrap();
        store.enable_async_gets();
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
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            hedge.write_all(&body).unwrap();
            // Keep the original socket alive until the winner consumes the
            // duplicate, proving winner selection actively cancels it.
            let mut byte = [0u8; 1];
            let _ = stalled.read(&mut byte);
        });

        let mut config = crate::config::Config::default_dev();
        config.object_store_on = true;
        config.object_store_endpoint = format!("127.0.0.1:{port}");
        config.object_store_bucket = "hedge-test".to_string();
        config.object_store_access_key = "key".to_string();
        config.object_store_secret_key = "secret".to_string();
        config.object_store_get_slots = 2;
        config.object_store_hedge_after_ms = 1;
        let mut budget = Budget::new(16 << 20);
        let mut store = OwnedObjectStore::new(&config, &mut budget, "blocks/").unwrap();
        store.enable_async_gets();

        let mut output = [0u8; 64];
        assert_eq!(store.get(&id, &mut output), Err(StoreError::NotReady));
        let deadline = store
            .next_hedge_deadline()
            .expect("pending read has a deadline");
        // Exercise the timer's strictly-due branch, not equality at an
        // `Instant` boundary whose precision differs across CI runners.
        store.issue_due_hedges(deadline + std::time::Duration::from_nanos(1));
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
