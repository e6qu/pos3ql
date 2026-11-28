//! A `Memoize` node as `nodeMemoize.c` runs it, its cache in
//! `plan_build_bytes` leases: each entry's key and rows are encoded in the
//! node's store, a hash table over the keys finds an entry, and a list in
//! least recently used order chooses what to evict once the entries and
//! rows PostgreSQL would hold exceed `hash_mem` (`cache_reduce_memory`).
//! Evicted and purged rows leave their bytes in the store until the dead
//! bytes outgrow the live ones, when the live entries are copied to a new
//! store.

use super::*;

/// No entry: the end of a chain or list.
const NONE: u32 = u32::MAX;

/// Dead store bytes tolerated before a compaction, beyond the live ones.
const COMPACT_SLACK: usize = 64 * 1024;

/// `ExecMemoize`'s states.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Status {
    /// `MEMO_CACHE_LOOKUP`: the next call looks the key up.
    Lookup,
    /// A miss reads its input's first row into `entry` (or past the
    /// cache when the entry was evicted as it was made).
    First { entry: u32 },
    /// `MEMO_CACHE_FETCH_NEXT_TUPLE`: a complete entry's rows return.
    Fetch,
    /// `MEMO_FILLING_CACHE`.
    Filling,
    /// `MEMO_CACHE_BYPASS_MODE`: the entry overflowed the cache.
    Bypass,
    /// `MEMO_END_OF_SCAN`.
    End,
}

/// A cache entry (`MemoizeEntry` with its `MemoizeKey`).
#[derive(Clone, Copy)]
struct Entry {
    hash: u32,
    /// The next entry of its bucket's chain, or of the free list.
    next: u32,
    /// Its neighbours in least recently used order.
    older: u32,
    newer: u32,
    /// The stored relation holding its key.
    key: u32,
    /// Its rows: the first stored relation and the row count.
    first: u32,
    rows: u32,
    /// `EMPTY_ENTRY_MEMORY_BYTES`, and the `CACHE_TUPLE_BYTES` of its rows.
    entry_bytes: usize,
    row_bytes: usize,
    complete: bool,
}

/// The cache: the entries, their hash table and the store of their keys
/// and rows.
struct Cache<'a> {
    store: MaterialStore<'a>,
    entries: NodeList<'a, Entry>,
    buckets: NodeList<'a, u32>,
    /// The least and the most recently used entry.
    oldest: u32,
    newest: u32,
    /// Freed entries, chained through `next`.
    free: u32,
    live: usize,
    /// `mem_used`.
    used: usize,
    /// The store bytes live entries hold.
    live_bytes: usize,
}

/// A `Memoize` node's state between rows.
pub(super) struct MemoRecord<'a> {
    status: Status,
    cache: Option<Cache<'a>>,
    /// Where its cache lives.
    space: Space<'a>,
    /// The entry being read or filled.
    entry: u32,
    /// The next row of a complete entry a read returns.
    at: u32,
    mark: Option<ArenaMark>,
    pub(super) stats: crate::sql::query::instrument::MemoizeStats,
}

impl<'a> MemoRecord<'a> {
    pub(super) fn new(space: Space<'a>) -> Self {
        Self {
            status: Status::Lookup,
            cache: None,
            space,
            entry: NONE,
            at: 0,
            mark: None,
            stats: Default::default(),
        }
    }

    /// `ExecReScanMemoize`: the next read looks up the new key.
    pub(super) fn rescan(&mut self) {
        self.status = Status::Lookup;
        self.entry = NONE;
        self.mark = None;
    }
}

fn memo_record<'r, 'a>(records: &'r mut [Record<'a>], id: u32) -> &'r mut MemoRecord<'a> {
    let Record::Memoize(record) = &mut records[id as usize] else {
        unreachable!("a Memoize's record");
    };
    record
}

impl<'a> Cache<'a> {
    fn new(space: &Space<'a>) -> Result<Self, SqlError> {
        let mut buckets = space.list::<u32>().map_err(store_full)?;
        buckets.reserve_exact(64).map_err(store_full)?;
        for _ in 0..64 {
            buckets.push_reserved(NONE).map_err(store_full)?;
        }
        Ok(Self {
            store: MaterialStore::new(space)?,
            entries: space.list().map_err(store_full)?,
            buckets,
            oldest: NONE,
            newest: NONE,
            free: NONE,
            live: 0,
            used: 0,
            live_bytes: 0,
        })
    }

    fn entry(&mut self, id: u32) -> &mut Entry {
        &mut self.entries.as_mut_slice()[id as usize]
    }

    fn bucket(&self, hash: u32) -> usize {
        hash as usize & (self.buckets.len() - 1)
    }

    /// Makes `id` the most recently used entry.
    fn touch(&mut self, id: u32) {
        self.unlink(id);
        let newest = self.newest;
        let entry = self.entry(id);
        entry.older = newest;
        entry.newer = NONE;
        if newest == NONE {
            self.oldest = id;
        } else {
            self.entry(newest).newer = id;
        }
        self.newest = id;
    }

    fn unlink(&mut self, id: u32) {
        let Entry { older, newer, .. } = *self.entry(id);
        if older == NONE {
            if self.oldest == id {
                self.oldest = newer;
            }
        } else {
            self.entry(older).newer = newer;
        }
        if newer == NONE {
            if self.newest == id {
                self.newest = older;
            }
        } else {
            self.entry(newer).older = older;
        }
        let entry = self.entry(id);
        entry.older = NONE;
        entry.newer = NONE;
    }

    /// The bytes of `id`'s key and rows in the store.
    fn stored_bytes(&self, id: u32, width: usize) -> usize {
        let entry = self.entries.as_slice()[id as usize];
        self.store.bytes_of(entry.key as usize, 1)
            + self
                .store
                .bytes_of(entry.first as usize, entry.rows as usize * width)
    }

    /// `entry_purge_tuples`.
    fn purge(&mut self, id: u32, width: usize) {
        let entry = self.entries.as_slice()[id as usize];
        self.live_bytes -= self
            .store
            .bytes_of(entry.first as usize, entry.rows as usize * width);
        self.used -= entry.row_bytes;
        let end = self.store.entries.len() as u32;
        let entry = self.entry(id);
        entry.rows = 0;
        entry.first = end;
        entry.row_bytes = 0;
        entry.complete = false;
    }

    /// `remove_cache_entry`.
    fn remove(&mut self, id: u32, width: usize) {
        self.purge(id, width);
        self.unlink(id);
        let entry = self.entries.as_slice()[id as usize];
        self.live_bytes -= self.store.bytes_of(entry.key as usize, 1);
        self.used -= entry.entry_bytes;
        let bucket = self.bucket(entry.hash);
        let mut link = self.buckets.as_slice()[bucket];
        if link == id {
            self.buckets.as_mut_slice()[bucket] = entry.next;
        } else {
            while link != NONE {
                let next = self.entries.as_slice()[link as usize].next;
                if next == id {
                    self.entry(link).next = entry.next;
                    break;
                }
                link = next;
            }
        }
        let free = self.free;
        self.entry(id).next = free;
        self.free = id;
        self.live -= 1;
    }

    /// `cache_reduce_memory`: evicts the least recently used entries until
    /// the cache fits `limit`; false when `special` was evicted.
    fn reduce(
        &mut self,
        special: u32,
        limit: usize,
        width: usize,
        stats: &mut crate::sql::query::instrument::MemoizeStats,
    ) -> bool {
        stats.peak = stats.peak.max(self.used);
        let mut intact = true;
        while self.oldest != NONE {
            let id = self.oldest;
            intact &= id != special;
            self.remove(id, width);
            stats.evictions += 1;
            if self.used <= limit {
                break;
            }
        }
        intact
    }

    /// Copies the live entries to a new store once dead bytes outgrow
    /// them; `filling`'s rows go last, so the rows it stores next follow
    /// them.
    fn compact(&mut self, filling: u32, width: usize) -> Result<(), SqlError> {
        // Compacting arena lists only takes more of the arena.
        if !self.store.releases() || self.store.held_bytes() <= 2 * self.live_bytes + COMPACT_SLACK
        {
            return Ok(());
        }
        let mut store = MaterialStore::new(&self.store.entries.space())?;
        for pass in [false, true] {
            let mut id = self.oldest;
            while id != NONE {
                let entry = self.entries.as_slice()[id as usize];
                if (id == filling) == pass {
                    let key = store.copy_from(&self.store, entry.key as usize, 1)?;
                    let first = store.copy_from(
                        &self.store,
                        entry.first as usize,
                        entry.rows as usize * width,
                    )?;
                    let entry = self.entry(id);
                    entry.key = key;
                    entry.first = first;
                }
                id = entry.newer;
            }
        }
        self.store = store;
        Ok(())
    }

    /// Doubles the buckets once the entries outnumber them.
    fn grow(&mut self) -> Result<(), SqlError> {
        if self.live < self.buckets.len() {
            return Ok(());
        }
        let size = self.buckets.len() * 2;
        self.buckets.reserve_exact(size).map_err(store_full)?;
        self.buckets.as_mut_slice().fill(NONE);
        while self.buckets.len() < size {
            self.buckets.push_reserved(NONE).map_err(store_full)?;
        }
        let mut id = self.oldest;
        while id != NONE {
            let entry = self.entries.as_slice()[id as usize];
            let bucket = self.bucket(entry.hash);
            self.entry(id).next = self.buckets.as_slice()[bucket];
            self.buckets.as_mut_slice()[bucket] = id;
            id = entry.newer;
        }
        Ok(())
    }
}

impl<'a> Step<'_, '_, 'a> {
    /// A `Memoize` node (`ExecMemoize`): the rows of its input for the
    /// bound outer row's key, from its cache when an earlier read stored
    /// them all.
    pub(super) fn memoize(
        &mut self,
        id: u32,
        input: u32,
        keys: &[Member],
        single_row: bool,
        binary: bool,
    ) -> Result<bool, SqlError> {
        let cx = self.cx;
        let relids = cx.model.plan.path(input).relids;
        let width = relation_count(relids, cx.scope.n);
        let limit = cx.model.plan.settings.hash_mem_bytes as usize;
        loop {
            let record = memo_record(self.records, id);
            match record.status {
                Status::End => return Ok(false),
                Status::Lookup => {
                    let mut key = [Datum::Null; crate::sql::query::index_paths::joins::MAX_KEYS];
                    for (slot, member) in key.iter_mut().zip(keys) {
                        *slot = cx.value(
                            self.bound,
                            (usize::from(member.rel), usize::from(member.column)),
                        );
                    }
                    let key = &key[..keys.len()];
                    if record.cache.is_none() {
                        record.cache = Some(Cache::new(&record.space)?);
                    }
                    let (found, entry) = self.lookup(id, key, keys, binary, limit, width)?;
                    let record = memo_record(self.records, id);
                    let cache = record.cache.as_mut().expect("a cache");
                    if let Some(found) = found
                        && cache.entries.as_slice()[found as usize].complete
                    {
                        record.stats.hits += 1;
                        record.entry = found;
                        record.at = 0;
                        record.status = Status::Fetch;
                        continue;
                    }
                    record.stats.misses += 1;
                    if let Some(found) = found {
                        // A scan of the entry stopped before its input
                        // ended: its rows are read again.
                        cache.purge(found, width);
                        cache.compact(found, width)?;
                    }
                    record.status = Status::First { entry };
                    self.rescan(input);
                }
                Status::First { entry } => {
                    if !self.next(input)? {
                        let record = memo_record(self.records, id);
                        if entry != NONE {
                            record
                                .cache
                                .as_mut()
                                .expect("a cache")
                                .entry(entry)
                                .complete = true;
                        }
                        record.status = Status::End;
                        return Ok(false);
                    }
                    let stored = entry != NONE && self.store_row(id, entry, input, limit, width)?;
                    let record = memo_record(self.records, id);
                    if stored {
                        record
                            .cache
                            .as_mut()
                            .expect("a cache")
                            .entry(entry)
                            .complete = single_row;
                        record.entry = entry;
                        record.status = Status::Filling;
                    } else {
                        record.stats.overflows += 1;
                        record.status = Status::Bypass;
                    }
                    return Ok(true);
                }
                Status::Fetch => {
                    let cache = record.cache.as_ref().expect("a cache");
                    let entry = cache.entries.as_slice()[record.entry as usize];
                    if record.at >= entry.rows {
                        record.status = Status::End;
                        return Ok(false);
                    }
                    Step::release(cx.arena, self.keep, &mut record.mark);
                    let first = entry.first as usize + record.at as usize * width;
                    record.at += 1;
                    cache.store.load(first, width, cx, self.bound)?;
                    return Ok(true);
                }
                Status::Filling => {
                    let entry = record.entry;
                    if !self.next(input)? {
                        let record = memo_record(self.records, id);
                        record
                            .cache
                            .as_mut()
                            .expect("a cache")
                            .entry(entry)
                            .complete = true;
                        record.status = Status::End;
                        return Ok(false);
                    }
                    let record = memo_record(self.records, id);
                    if record
                        .cache
                        .as_mut()
                        .expect("a cache")
                        .entry(entry)
                        .complete
                    {
                        return Err(crate::sql_err!(
                            crate::sql::eval::sqlstate::INTERNAL_ERROR,
                            "cache entry already complete"
                        ));
                    }
                    if !self.store_row(id, entry, input, limit, width)? {
                        let record = memo_record(self.records, id);
                        record.stats.overflows += 1;
                        record.status = Status::Bypass;
                    }
                    return Ok(true);
                }
                Status::Bypass => {
                    if !self.next(input)? {
                        memo_record(self.records, id).status = Status::End;
                        return Ok(false);
                    }
                    return Ok(true);
                }
            }
        }
    }

    /// `cache_lookup`: the entry of `key` when the cache holds one, made
    /// the most recently used, else a new empty entry (`NONE` when making
    /// it evicted it).
    fn lookup(
        &mut self,
        id: u32,
        key: &[Datum<'a>],
        members: &[Member],
        binary: bool,
        limit: usize,
        width: usize,
    ) -> Result<(Option<u32>, u32), SqlError> {
        let cx = self.cx;
        // `MemoizeHash_hash`: each key's hash rotated in, then mixed.
        let mut hash = 0u32;
        for value in key {
            hash = hash.rotate_left(1);
            if value.is_null() {
                continue;
            }
            hash ^= datum_hash(value).ok_or_else(|| {
                crate::sql_err!(
                    crate::sql::eval::sqlstate::FEATURE_NOT_SUPPORTED,
                    "a Memoize key of this type is not supported"
                )
            })?;
        }
        let hash = crate::sql::query::simplehash::murmur32(hash);
        let record = memo_record(self.records, id);
        let cache = record.cache.as_mut().expect("a cache");
        let mut link = cache.buckets.as_slice()[cache.bucket(hash)];
        while link != NONE {
            let entry = cache.entries.as_slice()[link as usize];
            if entry.hash == hash {
                let mark = cx.arena.mark();
                let held = cache
                    .store
                    .load_values(entry.key as usize, key.len(), cx.arena)
                    .and_then(|held| cx.same_key(held, key, members, binary));
                // SAFETY: the decoded key was only compared.
                unsafe { cx.arena.rewind_to(mark) };
                if held? {
                    cache.touch(link);
                    return Ok((Some(link), link));
                }
            }
            link = entry.next;
        }
        // `EMPTY_ENTRY_MEMORY_BYTES`: the entry, its key and the key's
        // tuple.
        let key_len = crate::sql::query::postgres_memory::minimal_tuple_len_with_tids(
            None,
            key.iter().copied(),
            0,
        )
        .ok_or_else(|| {
            crate::sql_err!(
                crate::sql::eval::sqlstate::FEATURE_NOT_SUPPORTED,
                "a Memoize key with no PostgreSQL size here is not supported"
            )
        })?;
        let stored = cache.store.save_values(cx, key)?;
        let entry = Entry {
            hash,
            next: NONE,
            older: NONE,
            newer: NONE,
            key: stored,
            first: cache.store.next_entry()?,
            rows: 0,
            entry_bytes: 24 + 24 + key_len,
            row_bytes: 0,
            complete: false,
        };
        let made = if cache.free == NONE {
            let made = u32::try_from(cache.entries.len()).map_err(|_| {
                crate::sql_err!(
                    crate::sql::eval::sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "a Memoize node holds more than 4294967295 entries"
                )
                .plan_build_exhausted()
            })?;
            if cache.entries.len() == cache.entries.capacity_entries() {
                let grown = (cache.entries.len() * 2).max(16);
                cache.entries.reserve_exact(grown).map_err(store_full)?;
            }
            cache.entries.push_reserved(entry).map_err(store_full)?;
            made
        } else {
            let made = cache.free;
            cache.free = cache.entries.as_slice()[made as usize].next;
            *cache.entry(made) = entry;
            made
        };
        cache.live += 1;
        let bucket = cache.bucket(hash);
        cache.entry(made).next = cache.buckets.as_slice()[bucket];
        cache.buckets.as_mut_slice()[bucket] = made;
        cache.touch(made);
        cache.used += entry.entry_bytes;
        cache.live_bytes += cache.stored_bytes(made, width);
        record.stats.peak = record.stats.peak.max(cache.used);
        cache.grow()?;
        if cache.used > limit {
            let intact = cache.reduce(made, limit, width, &mut record.stats);
            cache.compact(if intact { made } else { NONE }, width)?;
            if !intact {
                return Ok((None, NONE));
            }
        }
        Ok((None, made))
    }

    /// `cache_store_tuple`: stores the bound input row in `entry`; false
    /// when making room evicted the entry itself.
    fn store_row(
        &mut self,
        id: u32,
        entry: u32,
        input: u32,
        limit: usize,
        width: usize,
    ) -> Result<bool, SqlError> {
        let cx = self.cx;
        let relids = cx.model.plan.path(input).relids;
        // `CACHE_TUPLE_BYTES`: the cached tuple and its list cell.
        let size = 16 + cx.tuple_len(self.bound, input)?;
        let record = memo_record(self.records, id);
        let cache = record.cache.as_mut().expect("a cache");
        let first = cache.store.save(cx, self.bound, relids)?;
        debug_assert_eq!(
            first,
            cache.entries.as_slice()[entry as usize].first
                + cache.entries.as_slice()[entry as usize].rows * width as u32,
            "an entry's rows are stored together"
        );
        cache.live_bytes += cache.store.bytes_of(first as usize, width);
        cache.used += size;
        let stored = cache.entry(entry);
        stored.rows += 1;
        stored.row_bytes += size;
        record.stats.peak = record.stats.peak.max(cache.used);
        if cache.used > limit {
            let intact = cache.reduce(entry, limit, width, &mut record.stats);
            cache.compact(if intact { entry } else { NONE }, width)?;
            return Ok(intact);
        }
        Ok(true)
    }
}
