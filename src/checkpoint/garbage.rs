//! Resumable file retention and deletion. Durable writers stay paused while
//! the captured keep-set is used; foreground row edits remain in memory.

use super::*;

#[derive(Clone, Copy)]
#[allow(
    clippy::large_enum_variant,
    reason = "listing cursors are startup-reserved"
)]
pub(super) enum FileGcProgress {
    Unloaded,
    Backups { cursor: ListCursor },
    Manifests { index: usize },
    Walk { position: usize },
    List { cursor: ListCursor },
    Delete { next: Option<ListCursor> },
}

impl Checkpointer {
    pub(super) fn reset_file_gc(&mut self) {
        self.file_io.cancel();
        self.file_gc = FileGcProgress::Unloaded;
        self.file_index_walk = None;
        self.block_garbage_loaded = false;
        self.doomed_blocks.clear();
        self.legacy_garbage_loaded = false;
        self.garbage_scratch.clear();
    }

    pub(crate) fn file_io_ready(&self) -> bool {
        self.file_io.ready()
    }

    /// Errors before marking completes discard the incomplete decision. Once
    /// deletion starts, a failed request retains its exact key and cursor.
    pub(super) fn collect_block_garbage_batch(
        &mut self,
        storage: &Storage,
    ) -> Result<bool, SqlError> {
        let result = self.advance_file_gc(storage);
        if result.is_err() && !self.block_garbage_loaded {
            self.reset_file_gc();
            self.backup_roster_loaded = false;
        }
        result
    }

    fn capture_gc_roots(&mut self, storage: &Storage) -> Result<(), SqlError> {
        let mut keep = BlockKeepSet::new(&mut self.roster_scratch, &mut self.garbage_seen);
        // A merge mid-flight has written blocks no published roster names
        // yet; sweeping them would destroy the job's progress.
        if self.merge_job.is_some() {
            for id in self.merge_writer.roster_so_far() {
                keep.insert(*id)?;
            }
        }
        storage.copy_retained_spill_roots(true, &mut self.garbage_sst_roots)?;
        // A completed merge can finish after another manifest was
        // captured. It still owns its graph until a later publication
        // installs it or its source pair is replaced.
        let completed = self
            .merge_done
            .iter()
            .filter_map(|done| done.as_ref()?.merged);
        let finishing = self.merge_job.as_ref().and_then(|job| match job.phase {
            MergePhase::Finish { merged } => merged,
            _ => None,
        });
        for prev in self
            .prev_ssts
            .iter()
            .flat_map(SlotList::iter)
            .copied()
            .chain(completed)
            .chain(finishing)
        {
            if self.garbage_sst_roots.len() == self.garbage_sst_roots.capacity() {
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "garbage collection SST roots exceed fixed scratch capacity"
                ));
            }
            self.garbage_sst_roots.push(prev.handle);
        }
        self.garbage_sst_roots
            .sort_unstable_by_key(|handle| handle.roster);
        self.garbage_sst_roots.dedup_by_key(|handle| handle.roster);
        for h in self.garbage_sst_roots.iter().copied() {
            keep.insert_root(h.roster, BlockType::SstRoster)?;
        }
        // A schema change can make a captured manifest's local install
        // inapplicable. Tables still own their older installed files.
        // Stream missing roots without enlarging the reserved inventory.
        for slot in 0..storage.physical_table_count() {
            let table = storage.table(slot);
            if !table.live()
                || table.definition().persistence == crate::storage::RelationPersistence::Temporary
            {
                continue;
            }
            let count = table.spill_generations().len();
            for index in 0..count {
                let handle = table
                    .spill_generations()
                    .handle(index)
                    .expect("installed generation position");
                if self
                    .garbage_sst_roots
                    .binary_search_by_key(&handle.roster, |h| h.roster)
                    .is_err()
                {
                    keep.insert_root(handle.roster, BlockType::SstRoster)?;
                }
            }
        }
        if let Some(job) = &self.merge_job {
            for source in [job.old0.handle, job.old1.handle] {
                if self
                    .garbage_sst_roots
                    .binary_search_by_key(&source.roster, |h| h.roster)
                    .is_err()
                {
                    keep.insert_root(source.roster, BlockType::SstRoster)?;
                }
            }
        }
        storage.copy_retained_index_roots(&mut self.garbage_index_roots)?;
        for slot in 0..storage.physical_table_count() {
            for binding in 0..storage.value_binding_count(slot) {
                let Some(handle) = storage.value_binding_handle(slot, binding) else {
                    continue;
                };
                if self.garbage_index_roots.contains(&handle.roster) {
                    continue;
                }
                if self.garbage_index_roots.len() == self.garbage_index_roots.capacity() {
                    return Err(sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "garbage collection index roots exceed fixed scratch capacity"
                    ));
                }
                self.garbage_index_roots.push(handle.roster);
            }
        }
        self.garbage_index_roots.sort_unstable();
        self.garbage_index_roots.dedup();
        for roster in self.garbage_index_roots.iter().copied() {
            keep.insert_root(roster, BlockType::ValueIndexRoster)?;
        }
        Ok(())
    }

    fn advance_file_gc(&mut self, storage: &Storage) -> Result<bool, SqlError> {
        match self.file_gc {
            FileGcProgress::Unloaded => {
                self.capture_gc_roots(storage)?;
                self.file_index_walk = None;
                if self.backup_roster_loaded {
                    self.file_gc = FileGcProgress::Manifests { index: 0 };
                } else {
                    self.backup_scratch.clear();
                    self.file_gc = FileGcProgress::Backups {
                        cursor: ListCursor::START,
                    };
                }
            }
            FileGcProgress::Backups { cursor } => {
                let names = &mut self.backup_scratch;
                let mut invalid = false;
                let Poll::Ready(next) =
                    self.file_io
                        .list("backups/", cursor, maintenance::PAGE_KEYS, |key| {
                            let Some(name) = key
                                .strip_prefix("backups/")
                                .and_then(|rest| rest.strip_suffix("/manifest"))
                            else {
                                return;
                            };
                            if validate_backup_name(name).is_err()
                                || names.len() == names.capacity()
                            {
                                invalid = true;
                            } else {
                                names.push(StackStr::from_str(name));
                            }
                        })?
                else {
                    return Ok(false);
                };
                if invalid {
                    return Err(sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "backup roster exceeds max_backups ({}) or contains an invalid backup name",
                        names.capacity()
                    ));
                }
                if let Some(cursor) = next {
                    self.file_gc = FileGcProgress::Backups { cursor };
                } else {
                    names.sort_unstable_by(|a, b| a.as_str().cmp(b.as_str()));
                    names.dedup_by(|a, b| a.as_str() == b.as_str());
                    self.backup_roster_loaded = true;
                    self.file_gc = FileGcProgress::Manifests { index: 0 };
                }
            }
            FileGcProgress::Manifests { index } => {
                if index > self.backup_scratch.len() {
                    self.file_gc = FileGcProgress::Walk { position: 0 };
                    return Ok(false);
                }
                if index != 0 || self.manifest_present {
                    let key = if index == 0 {
                        StackStr::<128>::from_str(MANIFEST_KEY)
                    } else {
                        stack_format!(
                            128,
                            "backups/{}/manifest",
                            self.backup_scratch[index - 1].as_str()
                        )
                    };
                    let Poll::Ready(bytes) = self.file_io.get(key.as_str())? else {
                        return Ok(false);
                    };
                    let mut keep = BlockKeepSet {
                        blocks: &mut self.roster_scratch,
                        seen: &mut self.garbage_seen,
                    };
                    retain_manifest_blocks(&mut keep, bytes)?;
                }
                self.file_gc = FileGcProgress::Manifests { index: index + 1 };
            }
            FileGcProgress::Walk { mut position } => {
                // At most one block's metadata per turn. Leaf identities need
                // no GET: the complete row roster already names their extents.
                while position < self.roster_scratch.len()
                    && self.roster_scratch[position].1.is_none()
                {
                    position += 1;
                }
                if position == self.roster_scratch.len() {
                    self.roster_scratch.sort_unstable_by_key(|(id, _)| *id);
                    self.block_garbage_loaded = true;
                    self.file_gc = FileGcProgress::List {
                        cursor: ListCursor::START,
                    };
                    return Ok(false);
                }
                self.file_gc = FileGcProgress::Walk { position };
                let (id, kind) = self.roster_scratch[position];
                self.sst_arena.reset();
                let scratch = self
                    .sst_arena
                    .alloc_slice_with(crate::store::MAX_PAYLOAD, |_| 0u8)
                    .map_err(|_| sql_err!(SQLSTATE_IO, "gc scratch"))?;
                let mut keep = BlockKeepSet {
                    blocks: &mut self.roster_scratch,
                    seen: &mut self.garbage_seen,
                };
                if kind == Some(BlockType::SstRoster) {
                    let Poll::Ready((len, kind)) =
                        read_gc_block(&mut self.file_io, &self.blocks, &id, scratch)?
                    else {
                        return Ok(false);
                    };
                    if kind != BlockType::SstRoster || !len.is_multiple_of(32) {
                        return Err(sql_err!(
                            SQLSTATE_IO,
                            "invalid saved row roster type or length"
                        ));
                    }
                    for bytes in scratch[..len].as_chunks::<32>().0 {
                        keep.insert(BlockId(*bytes))?;
                    }
                } else {
                    let walk = self
                        .file_index_walk
                        .get_or_insert_with(|| crate::store::ValueRosterWalk::new(id));
                    let mut read_error = None;
                    let result = walk.step(
                        &mut |id, into| match read_gc_block(
                            &mut self.file_io,
                            &self.blocks,
                            id,
                            into,
                        ) {
                            Ok(Poll::Ready(block)) => Ok(block),
                            Ok(Poll::Pending) => Err(StoreError::NotReady),
                            Err(error) => {
                                read_error = Some(error);
                                Err(StoreError::Unavailable)
                            }
                        },
                        scratch,
                        &mut |id, _| keep.insert(id).is_ok(),
                    );
                    if let Some(error) = read_error {
                        return Err(error);
                    }
                    match result {
                        Ok(crate::store::ValueRosterStep::Pending)
                        | Err(crate::store::ValueIndexError::Store(StoreError::NotReady)) => {
                            return Ok(false);
                        }
                        Ok(crate::store::ValueRosterStep::Done) => self.file_index_walk = None,
                        Ok(crate::store::ValueRosterStep::Full) => {
                            return Err(sql_err!(
                                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                                "block garbage collection keep-set exceeds checkpoint_live_blocks ({})",
                                keep.capacity()
                            ));
                        }
                        Err(error) => {
                            return Err(sql_err!(
                                SQLSTATE_IO,
                                "corrupt persistent value-index roster: {:?}",
                                error
                            ));
                        }
                    }
                }
                self.file_gc = FileGcProgress::Walk {
                    position: position + 1,
                };
            }
            FileGcProgress::List { cursor } => {
                let keep = &self.roster_scratch;
                let doomed = &mut self.doomed_blocks;
                let limit = maintenance::PAGE_KEYS.min(doomed.capacity());
                let mut invalid = false;
                let Poll::Ready(next) = self.file_io.list("blocks/", cursor, limit, |key| {
                    let Some(id) = key
                        .strip_prefix("blocks/")
                        .and_then(|hex| parse_block_id(hex).ok())
                    else {
                        invalid = true;
                        return;
                    };
                    if keep.binary_search_by_key(&id, |(known, _)| *known).is_err() {
                        doomed.push(StackStr::from_str(key));
                    }
                })?
                else {
                    return Ok(false);
                };
                if invalid {
                    doomed.clear();
                    return Err(sql_err!(SQLSTATE_IO, "unrecognized immutable block key"));
                }
                self.file_gc = FileGcProgress::Delete { next };
            }
            FileGcProgress::Delete { next } => {
                for _ in 0..self.delete_objects_per_beat.min(self.doomed_blocks.len()) {
                    let key = *self.doomed_blocks.last().expect("staged block deletion");
                    if self.file_io.delete(key.as_str())?.is_pending() {
                        return Ok(false);
                    }
                    self.doomed_blocks.pop();
                }
                if self.doomed_blocks.is_empty() {
                    if let Some(cursor) = next {
                        self.file_gc = FileGcProgress::List { cursor };
                    } else {
                        self.file_gc = FileGcProgress::Unloaded;
                        self.block_garbage_loaded = false;
                        return Ok(!storage.take_retained_file_reclamation());
                    }
                }
            }
        }
        Ok(false)
    }
}

/// Reuse cache bytes without consuming a statement's pending request. Cache
/// misses use the cleanup client's own signed request and fixed response buffer.
fn read_gc_block(
    io: &mut MetadataIo,
    blocks: &std::sync::Arc<Mutex<TieredStore<OwnedObjectStore>>>,
    id: &BlockId,
    into: &mut [u8],
) -> Result<Poll<(usize, BlockType)>, SqlError> {
    if !io.busy()
        && let Some(block) = blocks
            .lock()
            .expect("block store lock poisoned")
            .get_cached(id, into)
            .map_err(|error| sql_err!(SQLSTATE_IO, "gc cached block: {:?}", error))?
    {
        return Ok(Poll::Ready(block));
    }
    let mut hex = [0; 64];
    id.write_key(&mut hex);
    let key = stack_format!(
        128,
        "blocks/{}",
        core::str::from_utf8(&hex).expect("hex block key")
    );
    let Poll::Ready(bytes) = io.get(key.as_str())? else {
        return Ok(Poll::Pending);
    };
    let block = crate::store::decode(bytes, true)
        .map_err(|error| sql_err!(SQLSTATE_IO, "gc block verification: {:?}", error))?;
    if block.id != *id {
        return Err(sql_err!(SQLSTATE_IO, "gc block identity mismatch"));
    }
    if into.len() < block.payload.len() {
        return Err(sql_err!(SQLSTATE_IO, "gc block buffer too small"));
    }
    into[..block.payload.len()].copy_from_slice(block.payload);
    Ok(Poll::Ready((block.payload.len(), block.block_type)))
}
