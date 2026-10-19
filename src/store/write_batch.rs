//! One bounded writer emission retained until every immutable PUT completes.

use std::time::{Duration, Instant};

use crate::mem::budget::{Budget, BudgetError};
use crate::mem::buffer::FixedBuf;
use crate::mem::fixed_vec::FixedVec;
use crate::object_store::upload::{Condition, Ticket, UploadError, Uploads};

use super::{BlockId, BlockType, BlockWriter, HEADER_LEN, StoreError};

struct PendingBlock {
    id: BlockId,
    offset: usize,
    length: usize,
    ticket: Option<Ticket>,
    durable: bool,
}

/// Contains one complete writer operation, never an entire table. The writer
/// advances only after this emission fits; the owner consumes its result only
/// after every block is durable. Request errors keep the original bytes.
pub(crate) struct BlockWriteBatch {
    bytes: FixedBuf,
    blocks: FixedVec<PendingBlock>,
    sealed: bool,
    retry_at: Option<Instant>,
    object_puts: u64,
}

impl BlockWriteBatch {
    pub(crate) fn budget_bytes(byte_capacity: usize, object_capacity: usize) -> usize {
        byte_capacity.saturating_add(object_capacity.saturating_mul(size_of::<PendingBlock>()))
    }

    pub(crate) fn new(
        budget: &mut Budget,
        byte_capacity: usize,
        object_capacity: usize,
    ) -> Result<Self, BudgetError> {
        Ok(Self {
            bytes: FixedBuf::new(budget, "physical output emission bytes", byte_capacity)?,
            blocks: FixedVec::new(budget, "physical output emission objects", object_capacity)?,
            sealed: false,
            retry_at: None,
            object_puts: 0,
        })
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    pub(crate) fn ready(&self, uploads: &Uploads, now: Instant) -> bool {
        if self.retry_at.is_some_and(|deadline| deadline > now) {
            return false;
        }
        self.blocks.iter().all(|block| block.durable)
            || uploads.has_completion()
            || (uploads.has_capacity()
                && self
                    .blocks
                    .iter()
                    .any(|block| !block.durable && block.ticket.is_none()))
    }

    pub(crate) fn deadline(&self, uploads: &Uploads) -> Option<Instant> {
        if !uploads.admits_requests() {
            return if !self.is_empty()
                && (self.blocks.iter().all(|block| block.durable) || uploads.has_completion())
            {
                Some(Instant::now())
            } else {
                uploads.next_deadline()
            };
        }
        self.retry_at.or_else(|| {
            if self.ready(uploads, Instant::now()) && !self.is_empty() {
                Some(Instant::now())
            } else {
                uploads.next_deadline()
            }
        })
    }

    /// Explicit startup/manual draining waits on request readiness. Reactor
    /// callers use ready/deadline and keep serving other connections instead.
    pub(crate) fn wait_ready(&mut self, uploads: &mut Uploads) -> Result<(), UploadError> {
        let now = Instant::now();
        if self.ready(uploads, now) {
            return Ok(());
        }
        if let Some(deadline) = self.retry_at {
            uploads.wait_until(deadline).map_err(UploadError::Request)
        } else {
            uploads.wait_ready().map_err(UploadError::Request)
        }
    }

    pub(crate) fn object_puts(&self) -> u64 {
        self.object_puts
    }

    fn cancel_pending(&mut self, uploads: &mut Uploads) -> Result<(), UploadError> {
        for block in self.blocks.as_mut_slice() {
            if let Some(ticket) = block.ticket.take() {
                uploads.cancel(ticket)?;
            }
        }
        Ok(())
    }

    /// Discarded output has no published root. Canceling an uncertain PUT may
    /// leave an orphan object; it can never invalidate committed data.
    pub(crate) fn discard(&mut self, uploads: &mut Uploads) -> Result<(), UploadError> {
        self.cancel_pending(uploads)?;
        self.bytes.clear();
        self.blocks.clear();
        self.sealed = false;
        self.retry_at = None;
        Ok(())
    }

    pub(crate) fn drain(
        &mut self,
        uploads: &mut Uploads,
        now: Instant,
    ) -> Result<bool, UploadError> {
        self.sealed = true;
        if self.retry_at.is_some_and(|deadline| deadline > now) {
            return Ok(false);
        }
        self.retry_at = None;
        uploads.advance(now);
        for index in 0..self.blocks.len() {
            let Some(ticket) = self.blocks[index].ticket else {
                continue;
            };
            match uploads.take(ticket) {
                Ok(Some(_)) => {
                    self.blocks[index].ticket = None;
                    self.blocks[index].durable = true;
                }
                Ok(None) => {}
                Err(error) => {
                    self.blocks[index].ticket = None;
                    self.cancel_pending(uploads)?;
                    self.retry_at = Some(now + Duration::from_secs(1));
                    return Err(error);
                }
            }
        }
        for block in self.blocks.as_mut_slice() {
            if block.durable || block.ticket.is_some() {
                continue;
            }
            if !uploads.has_capacity() {
                break;
            }
            let mut key_buffer = [0u8; 128];
            let key = super::object::key_of("blocks/", &block.id, &mut key_buffer);
            let ticket = uploads.enqueue(
                key,
                &self.bytes.readable()[block.offset..block.offset + block.length],
                Condition::ContentAddressed,
            )?;
            self.object_puts = self.object_puts.saturating_add(1);
            block.ticket = Some(ticket);
        }
        Ok(self.blocks.iter().all(|block| block.durable))
    }

    pub(crate) fn consume(&mut self) {
        assert!(
            self.blocks.iter().all(|block| block.durable),
            "output must be durable before consumption"
        );
        self.bytes.clear();
        self.blocks.clear();
        self.sealed = false;
        self.retry_at = None;
    }
}

impl BlockWriter for BlockWriteBatch {
    fn write_block(
        &mut self,
        payload: &[u8],
        block_type: BlockType,
        lsn: u64,
    ) -> Result<BlockId, StoreError> {
        assert!(!self.sealed, "one immutable writer emission at a time");
        let length = HEADER_LEN
            .checked_add(payload.len())
            .ok_or(StoreError::WriteBatchBytesExhausted)?;
        let limit = if self.blocks.len() == self.blocks.capacity() {
            Some(StoreError::WriteBatchObjectsExhausted)
        } else if length > self.bytes.capacity() - self.bytes.len() {
            Some(StoreError::WriteBatchBytesExhausted)
        } else {
            None
        };
        if let Some(error) = limit {
            let id = BlockId::for_block(block_type, payload);
            return if self.blocks.iter().any(|block| block.id == id) {
                Ok(id)
            } else {
                Err(error)
            };
        }
        let offset = self.bytes.len();
        let (id, encoded) = super::encode(payload, block_type, lsn, self.bytes.writable())?;
        if self.blocks.iter().any(|block| block.id == id) {
            return Ok(id);
        }
        self.bytes.advance(encoded);
        self.blocks
            .push(PendingBlock {
                id,
                offset,
                length: encoded,
                ticket: None,
                durable: false,
            })
            .unwrap_or_else(|_| unreachable!("preflighted output object capacity"));
        Ok(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem::guard::forbid_alloc;
    use crate::object_store::Precondition;
    use crate::object_store::upload::tests::{config, reply, request};
    use std::net::TcpListener;
    use std::sync::mpsc;

    #[test]
    fn parallel_credentials_draining_keeps_unstarted_physical_bytes_until_resume() {
        let mut config = config(1);
        config.object_store_sim = true;
        config.object_store_bucket = format!("credential-write-drain-{}", std::process::id());
        crate::object_store::sim::drop_namespace(&config.object_store_bucket);
        let namespace = crate::object_store::sim::open_namespace(&config.object_store_bucket, 292);
        let bytes = 2 * (HEADER_LEN + 4);
        let mut budget = Budget::new(
            BlockWriteBatch::budget_bytes(bytes, 2)
                + Uploads::budget_bytes(&config, 1, super::super::BLOCK_SIZE),
        );
        let mut output = BlockWriteBatch::new(&mut budget, bytes, 2).unwrap();
        let mut uploads = Uploads::new(&config, 1, super::super::BLOCK_SIZE, &mut budget).unwrap();
        let first = output.write_block(b"keep", BlockType::SstData, 0).unwrap();
        let second = output.write_block(b"also", BlockType::SstData, 0).unwrap();
        assert!(!output.drain(&mut uploads, Instant::now()).unwrap());
        uploads.set_admission(crate::object_store::RequestAdmission::Draining);
        assert!(!output.drain(&mut uploads, Instant::now()).unwrap());
        assert!(!uploads.busy());
        assert!(output.blocks[0].durable && !output.blocks[1].durable);
        assert!(!output.ready(&uploads, Instant::now()));
        assert!(
            output.deadline(&uploads).is_none(),
            "paused output must not busy-poll"
        );
        assert_eq!(output.bytes.len(), bytes);
        assert_eq!(output.blocks[0].id, first);
        assert_eq!(output.blocks[1].id, second);
        assert_eq!(namespace.borrow().object_count(), 1);
        uploads.set_admission(crate::object_store::RequestAdmission::Open);
        assert!(output.ready(&uploads, Instant::now()));
        let mut completed = false;
        for _ in 0..8 {
            if output.drain(&mut uploads, Instant::now()).unwrap() {
                completed = true;
                break;
            }
        }
        assert!(completed);
        assert_eq!(namespace.borrow().object_count(), 2);
        assert!(namespace.borrow().blind_overwrites.is_empty());
        output.consume();
        assert!(output.is_empty());
        crate::object_store::sim::drop_namespace(&config.object_store_bucket);
    }

    #[test]
    fn parallel_physical_output_reserves_exact_capacity_and_deduplicates_typed_blocks() {
        let bytes = 2 * (HEADER_LEN + 4);
        let total = BlockWriteBatch::budget_bytes(bytes, 2);
        let mut budget = Budget::new(total);
        let mut output = BlockWriteBatch::new(&mut budget, bytes, 2).unwrap();
        assert_eq!(budget.used(), total);
        forbid_alloc(|| {
            let first = output.write_block(b"same", BlockType::SstData, 0).unwrap();
            assert_eq!(
                output.write_block(b"same", BlockType::SstData, 73).unwrap(),
                first
            );
            let second = output
                .write_block(b"same", BlockType::SstDataPaxColumnV1, 0)
                .unwrap();
            assert_ne!(first, second);
            assert_eq!(output.blocks.len(), 2);
            assert_eq!(output.bytes.len(), bytes);
            assert_eq!(
                output
                    .write_block(b"next", BlockType::SstData, 0)
                    .unwrap_err(),
                StoreError::WriteBatchObjectsExhausted
            );
        });
        let mut short = Budget::new(total - 1);
        assert!(BlockWriteBatch::new(&mut short, bytes, 2).is_err());
        let mut small_budget = Budget::new(BlockWriteBatch::budget_bytes(HEADER_LEN + 3, 1));
        let mut small = BlockWriteBatch::new(&mut small_budget, HEADER_LEN + 3, 1).unwrap();
        forbid_alloc(|| {
            assert_eq!(
                small
                    .write_block(b"same", BlockType::SstData, 0)
                    .unwrap_err(),
                StoreError::WriteBatchBytesExhausted
            );
            assert!(small.bytes.is_empty() && small.blocks.is_empty());
        });
    }

    #[test]
    fn parallel_physical_output_owns_bytes_and_waits_for_every_http_completion() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let config = config(listener.local_addr().unwrap().port());
        let (release, held) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut first, _) = listener.accept().unwrap();
            let (mut second, _) = listener.accept().unwrap();
            let first_bytes = request(&mut first, Precondition::None);
            let second_bytes = request(&mut second, Precondition::None);
            let first_block = super::super::decode(&first_bytes, true).unwrap();
            let second_block = super::super::decode(&second_bytes, true).unwrap();
            assert!(first_block.payload.iter().all(|byte| *byte == b'a'));
            assert!(second_block.payload.iter().all(|byte| *byte == b'b'));
            reply(&mut second, "second");
            held.recv_timeout(Duration::from_secs(5)).unwrap();
            reply(&mut first, "first");
        });
        let capacity = 2 * super::super::BLOCK_SIZE;
        let mut budget = Budget::new(
            BlockWriteBatch::budget_bytes(capacity, 2)
                + Uploads::budget_bytes(&config, 2, super::super::BLOCK_SIZE),
        );
        let mut output = BlockWriteBatch::new(&mut budget, capacity, 2).unwrap();
        let mut uploads = Uploads::new(&config, 2, super::super::BLOCK_SIZE, &mut budget).unwrap();
        let mut first = vec![b'a'; 192 * 1024];
        let mut second = vec![b'b'; 192 * 1024];
        forbid_alloc(|| {
            output.write_block(&first, BlockType::SstDataV2, 0).unwrap();
            output
                .write_block(&second, BlockType::SstDataV2, 0)
                .unwrap();
        });
        first.fill(b'x');
        second.fill(b'y');
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            assert!(!forbid_alloc(|| output.drain(&mut uploads, Instant::now())).unwrap());
            if output.blocks[1].durable {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "second physical PUT did not overlap the first"
            );
            forbid_alloc(|| uploads.wait_ready()).unwrap();
        }
        assert!(!output.blocks[0].durable);
        assert_eq!(output.object_puts(), 2);
        release.send(()).unwrap();
        loop {
            if forbid_alloc(|| output.drain(&mut uploads, Instant::now())).unwrap() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "physical PUT completion was lost"
            );
            forbid_alloc(|| output.wait_ready(&mut uploads)).unwrap();
        }
        forbid_alloc(|| output.consume());
        assert!(output.is_empty() && !uploads.busy());
        server.join().unwrap();
    }

    #[test]
    fn parallel_physical_output_retains_original_bytes_across_failed_attempts() {
        let mut config = config(1);
        config.object_store_sim = true;
        config.object_store_bucket = format!("physical-output-retry-{}", std::process::id());
        crate::object_store::sim::drop_namespace(&config.object_store_bucket);
        let namespace = crate::object_store::sim::open_namespace(&config.object_store_bucket, 222);
        let bytes = 2 * (HEADER_LEN + 4);
        let mut budget = Budget::new(
            BlockWriteBatch::budget_bytes(bytes, 2)
                + Uploads::budget_bytes(&config, 2, super::super::BLOCK_SIZE),
        );
        let mut output = BlockWriteBatch::new(&mut budget, bytes, 2).unwrap();
        let mut uploads = Uploads::new(&config, 2, super::super::BLOCK_SIZE, &mut budget).unwrap();
        let first = output.write_block(b"keep", BlockType::SstData, 0).unwrap();
        let second = output.write_block(b"also", BlockType::SstData, 0).unwrap();
        namespace.borrow_mut().faults.transient_per_mille = 1000;
        let start = Instant::now();
        let mut failed_at = None;
        for turn in 0..16 {
            let now = start + Duration::from_millis(turn * 250);
            match output.drain(&mut uploads, now) {
                Ok(false) => {}
                Ok(true) => panic!("failed uploads cannot complete the batch"),
                Err(_) => {
                    failed_at = Some(now);
                    break;
                }
            }
        }
        let failed_at = failed_at.expect("request attempts must surface their terminal failure");
        assert_eq!(output.blocks.len(), 2);
        assert_eq!(output.bytes.len(), bytes);
        assert_eq!(output.blocks[0].id, first);
        assert_eq!(output.blocks[1].id, second);
        for (index, expected) in [b"keep", b"also"].into_iter().enumerate() {
            let block = &output.blocks[index];
            assert_eq!(
                super::super::decode(
                    &output.bytes.readable()[block.offset..block.offset + block.length],
                    true
                )
                .unwrap()
                .payload,
                expected
            );
        }
        namespace.borrow_mut().faults.transient_per_mille = 0;
        let mut completed = false;
        for turn in 0..16 {
            if output
                .drain(
                    &mut uploads,
                    failed_at + Duration::from_secs(2) + Duration::from_millis(turn * 250),
                )
                .unwrap()
            {
                completed = true;
                break;
            }
        }
        assert!(
            completed,
            "retained bytes must complete after storage recovers"
        );
        assert_eq!(namespace.borrow().object_count(), 2);
        assert!(namespace.borrow().blind_overwrites.is_empty());
        output.consume();
        assert!(output.is_empty());
        crate::object_store::sim::drop_namespace(&config.object_store_bucket);
    }
}
