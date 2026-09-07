//! The authoritative, process-local block store for temporary relation spill.
//!
//! Temporary rows need the same bounded SST representation as durable rows,
//! but sending those blocks through the ordinary tier stack would publish
//! session-private data to object storage. This store is therefore a separate
//! preallocated file with a startup-sized identity index. It is recreated
//! empty whenever the engine starts, never fsynced, and never named by a
//! manifest. A full store fails loudly instead of evicting a live block.

use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::Path;

use crate::mem::budget::{Budget, BudgetError};
use crate::mem::fixed_map::FixedMap;

use super::{BLOCK_SIZE, BlockError, BlockId, BlockStore, BlockType, StoreError, decode, encode};

#[derive(Clone, Copy)]
struct Slot {
    id: BlockId,
    len: usize,
    keep: bool,
}

/// Setup errors remain distinct from read/write exhaustion: setup reports the
/// local path that could not be opened, while runtime capacity is a normal
/// bounded-store `Unavailable` result translated by the SST layer.
#[derive(Debug)]
pub(crate) enum EphemeralSetupError {
    Budget(BudgetError),
    Io(&'static str, std::io::Error),
    TooSmall,
}

impl std::fmt::Display for EphemeralSetupError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Budget(error) => write!(formatter, "{error}"),
            Self::Io(operation, error) => write!(formatter, "{operation}: {error}"),
            Self::TooSmall => write!(
                formatter,
                "temporary_spill_bytes is smaller than one block ({BLOCK_SIZE} bytes)"
            ),
        }
    }
}

impl std::error::Error for EphemeralSetupError {}

pub(crate) struct EphemeralBlockStore {
    file: File,
    slots: Box<[Option<Slot>]>,
    index: FixedMap<BlockId, usize>,
    scratch: Box<[u8]>,
    used: usize,
}

impl EphemeralBlockStore {
    pub(crate) fn budget_bytes(bytes: usize) -> usize {
        let slots = bytes / BLOCK_SIZE;
        slots * core::mem::size_of::<Option<Slot>>()
            + FixedMap::<BlockId, usize>::budget_bytes(slots)
            + BLOCK_SIZE
    }

    pub(crate) fn open(
        budget: &mut Budget,
        path: &Path,
        bytes: usize,
    ) -> Result<Self, EphemeralSetupError> {
        let slot_count = bytes / BLOCK_SIZE;
        if slot_count == 0 {
            return Err(EphemeralSetupError::TooSmall);
        }
        budget
            .draw_array(
                slot_count,
                core::mem::size_of::<Option<Slot>>(),
                "temporary spill slots",
            )
            .map_err(EphemeralSetupError::Budget)?;
        budget
            .draw_array(BLOCK_SIZE, 1, "temporary spill scratch")
            .map_err(EphemeralSetupError::Budget)?;
        let index = FixedMap::new(budget, "temporary spill index", slot_count)
            .map_err(EphemeralSetupError::Budget)?;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
            .map_err(|error| EphemeralSetupError::Io("open temporary spill", error))?;
        file.set_len(bytes as u64)
            .map_err(|error| EphemeralSetupError::Io("size temporary spill", error))?;
        Ok(Self {
            file,
            slots: vec![None; slot_count].into_boxed_slice(),
            index,
            scratch: vec![0; BLOCK_SIZE].into_boxed_slice(),
            used: 0,
        })
    }

    pub(crate) fn clear(&mut self) {
        self.index.clear();
        self.slots.fill(None);
        self.used = 0;
    }

    /// Compacts the file to exactly the blocks reachable from `handles`.
    /// Each SST roster names every stored constituent except the roster
    /// itself, so reachability is complete without inspecting index trees.
    pub(crate) fn retain(&mut self, handles: &[crate::store::SstHandle]) -> Result<(), StoreError> {
        for slot in self.slots[..self.used].iter_mut().flatten() {
            slot.keep = false;
        }
        for handle in handles {
            let roster_slot = *self.index.get(&handle.roster).ok_or(StoreError::NotFound)?;
            self.slots[roster_slot]
                .as_mut()
                .ok_or(StoreError::NotFound)?
                .keep = true;
            let roster_len = self.slots[roster_slot].ok_or(StoreError::NotFound)?.len;
            self.file
                .read_exact_at(
                    &mut self.scratch[..roster_len],
                    (roster_slot * BLOCK_SIZE) as u64,
                )
                .map_err(|_| StoreError::Unavailable)?;
            let roster = decode(&self.scratch[..roster_len], true)?;
            if roster.id != handle.roster
                || roster.block_type != BlockType::SstRoster
                || roster.payload.len() % 32 != 0
            {
                return Err(StoreError::Corrupt(BlockError::IdentityMismatch));
            }
            for encoded in roster.payload.as_chunks::<32>().0 {
                let slot = *self
                    .index
                    .get(&BlockId(*encoded))
                    .ok_or(StoreError::NotFound)?;
                self.slots[slot].as_mut().ok_or(StoreError::NotFound)?.keep = true;
            }
        }

        self.index.clear();
        let previous_used = self.used;
        let mut write_slot = 0usize;
        for read_slot in 0..previous_used {
            let Some(mut slot) = self.slots[read_slot] else {
                continue;
            };
            if !slot.keep {
                continue;
            }
            slot.keep = false;
            if read_slot != write_slot {
                self.file
                    .read_exact_at(
                        &mut self.scratch[..slot.len],
                        (read_slot * BLOCK_SIZE) as u64,
                    )
                    .map_err(|_| StoreError::Unavailable)?;
                self.file
                    .write_all_at(&self.scratch[..slot.len], (write_slot * BLOCK_SIZE) as u64)
                    .map_err(|_| StoreError::Unavailable)?;
            }
            self.slots[write_slot] = Some(slot);
            self.index
                .insert(slot.id, write_slot)
                .map_err(|_| StoreError::Unavailable)?;
            write_slot += 1;
        }
        self.slots[write_slot..previous_used].fill(None);
        self.used = write_slot;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn block_count(&self) -> usize {
        self.used
    }
}

impl BlockStore for EphemeralBlockStore {
    fn put(
        &mut self,
        payload: &[u8],
        block_type: BlockType,
        lsn: u64,
    ) -> Result<BlockId, StoreError> {
        let (id, len) = encode(payload, block_type, lsn, &mut self.scratch)?;
        if self.index.get(&id).is_some() {
            return Ok(id);
        }
        if self.used == self.slots.len() {
            return Err(StoreError::Unavailable);
        }
        let slot = self.used;
        self.file
            .write_all_at(&self.scratch[..len], (slot * BLOCK_SIZE) as u64)
            .map_err(|_| StoreError::Unavailable)?;
        self.index
            .insert(id, slot)
            .map_err(|_| StoreError::Unavailable)?;
        self.slots[slot] = Some(Slot {
            id,
            len,
            keep: false,
        });
        self.used += 1;
        Ok(id)
    }

    fn get(&mut self, id: &BlockId, into: &mut [u8]) -> Result<(usize, BlockType), StoreError> {
        let slot = *self.index.get(id).ok_or(StoreError::NotFound)?;
        let len = self.slots[slot].ok_or(StoreError::NotFound)?.len;
        self.file
            .read_exact_at(&mut self.scratch[..len], (slot * BLOCK_SIZE) as u64)
            .map_err(|_| StoreError::Unavailable)?;
        let block = decode(&self.scratch[..len], true)?;
        if block.id != *id {
            return Err(StoreError::Corrupt(BlockError::IdentityMismatch));
        }
        if into.len() < block.payload.len() {
            return Err(StoreError::BufferTooSmall);
        }
        into[..block.payload.len()].copy_from_slice(block.payload);
        Ok((block.payload.len(), block.block_type))
    }

    #[cfg(test)]
    fn contains(&mut self, id: &BlockId) -> Result<bool, StoreError> {
        Ok(self.index.get(id).is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temporary_blocks_are_bounded_reusable_and_cleared() {
        let path =
            std::env::temp_dir().join(format!("pos3ql-temporary-blocks-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let bytes = 2 * BLOCK_SIZE;
        let mut budget = Budget::new(EphemeralBlockStore::budget_bytes(bytes) + 4096);
        let mut store = EphemeralBlockStore::open(&mut budget, &path, bytes).unwrap();
        let first = store.put(b"first", BlockType::SstData, 1).unwrap();
        assert_eq!(store.put(b"first", BlockType::SstData, 1), Ok(first));
        store.put(b"second", BlockType::SstData, 2).unwrap();
        assert_eq!(
            store.put(b"third", BlockType::SstData, 3),
            Err(StoreError::Unavailable)
        );
        let mut output = [0u8; 16];
        assert_eq!(store.get(&first, &mut output).unwrap().0, 5);
        assert_eq!(&output[..5], b"first");
        store.clear();
        assert_eq!(store.block_count(), 0);
        assert_eq!(store.get(&first, &mut output), Err(StoreError::NotFound));
        std::fs::remove_file(path).unwrap();
    }
}
