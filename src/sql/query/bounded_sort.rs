//! A sort under a limit fed one row at a time, as PostgreSQL's tuplesort
//! takes rows (`tuplesort_puttuple_common`): rows arrive in an array until
//! the sort's memory model turns it into a bounded heap (past twice the
//! bound, `make_bounded_heap`), after which only the heap's rows are kept and
//! each later row is freed or replaces the heap's greatest. A sort that
//! never forms its heap keeps every row and sorts them as an unbounded one
//! does ([`super::instrument::SortTuples::sort_rows`]).
//!
//! The heap's rows live in two `plan_build_bytes` leases: a row replacing
//! another is appended, and once the replaced rows' bytes are half the
//! lease the kept rows move to the other lease. So a join of any size
//! under `ORDER BY ... LIMIT n` holds `n` rows, not its result.
//! <https://github.com/postgres/postgres/blob/REL_18_6/src/backend/utils/sort/tuplesort.c>

use core::cmp::Ordering;

use crate::mem::arena::{Arena, ArenaList};
use crate::mem::lease_pool::{LeaseError, MutableSlice};
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql::pg_qsort::{self, HeapOffer};
use crate::sql_err;

use super::arena_full;
use super::instrument::{SortSpace, SortTuples};
use super::postgres_memory::{SizeSource, TuplesortModel};

/// A row comparison of the sort's keys.
pub(super) type RowCompare<'c> = dyn FnMut(&[u8], &[u8]) -> Result<Ordering, SqlError> + 'c;

fn sort_full(error: LeaseError) -> SqlError {
    sql_err!(
        sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "a bounded sort exceeds plan_build_bytes: {}",
        error
    )
    .plan_build_exhausted()
}

/// The heap's rows: each heap entry's bytes in `bytes`, by entry id.
struct HeapRows {
    bytes: MutableSlice<u8>,
    spare: MutableSlice<u8>,
    /// Bytes of rows no entry holds any more.
    garbage: usize,
}

impl HeapRows {
    fn row<'s>(&'s self, entries: &[(u32, u32)], id: u32) -> &'s [u8] {
        let (start, length) = entries[id as usize];
        &self.bytes.as_slice()[start as usize..(start + length) as usize]
    }

    /// Stores `row` as entry `id`'s, freeing the bytes it held.
    fn store(&mut self, entries: &mut [(u32, u32)], id: u32, row: &[u8]) -> Result<(), SqlError> {
        let held = entries[id as usize].1 as usize;
        if self.bytes.len() + row.len() > self.bytes.capacity_entries()
            && self.garbage >= self.bytes.len() / 2
        {
            self.compact(entries)?;
        }
        let end = self.bytes.len() + row.len();
        if end > self.bytes.capacity_entries() {
            let grown = self
                .bytes
                .capacity_entries()
                .saturating_mul(2)
                .min(self.bytes.max_entries())
                .max(end);
            self.bytes.reserve_exact(grown).map_err(sort_full)?;
        }
        let (Ok(start), Ok(length), Ok(_)) = (
            u32::try_from(self.bytes.len()),
            u32::try_from(row.len()),
            u32::try_from(end),
        ) else {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "a bounded sort exceeds 4 GiB of rows"
            )
            .plan_build_exhausted());
        };
        self.bytes.extend_from_slice(row).map_err(sort_full)?;
        self.garbage += held;
        entries[id as usize] = (start, length);
        Ok(())
    }

    /// Moves the entries' rows to the spare lease, dropping freed bytes.
    fn compact(&mut self, entries: &mut [(u32, u32)]) -> Result<(), SqlError> {
        let live = self.bytes.len() - self.garbage;
        self.spare.clear();
        if live > self.spare.capacity_entries() {
            self.spare.reserve_exact(live).map_err(sort_full)?;
        }
        for entry in entries.iter_mut() {
            let (start, length) = *entry;
            let at = self.spare.len() as u32;
            self.spare
                .extend_from_slice(
                    &self.bytes.as_slice()[start as usize..(start + length) as usize],
                )
                .map_err(sort_full)?;
            *entry = (at, length);
        }
        core::mem::swap(&mut self.bytes, &mut self.spare);
        self.spare.clear();
        self.garbage = 0;
        Ok(())
    }
}

/// The bounded heap once formed: entry ids in heap order, greatest first,
/// and the running charge of the rows PostgreSQL's sort still holds.
struct Heap<'a> {
    order: &'a mut [u32],
    count: usize,
    entries: &'a mut [(u32, u32)],
    /// Each entry's row's charge.
    charges: &'a mut [usize],
    rows: HeapRows,
    charge: usize,
}

pub(super) struct BoundedSort<'t, 'a> {
    tuples: &'t SortTuples,
    source: SizeSource<'t>,
    bound: usize,
    model: Option<TuplesortModel>,
    /// The first row read, which decides how the sort copies rows.
    first: Option<&'a [u8]>,
    /// Rows in arrival order until the heap forms.
    early: ArenaList<'a, &'a [u8]>,
    heap: Option<Heap<'a>>,
    arena: &'a Arena,
}

impl<'t, 'a> BoundedSort<'t, 'a> {
    /// A sort keeping the `bound` first rows, `bound` at least one.
    pub(super) fn new(
        tuples: &'t SortTuples,
        source: SizeSource<'t>,
        bound: usize,
        arena: &'a Arena,
    ) -> Self {
        debug_assert!(bound > 0, "a limit of no rows reads nothing");
        Self {
            tuples,
            source,
            bound,
            model: None,
            first: None,
            early: ArenaList::new_persistent(arena),
            heap: None,
            arena,
        }
    }

    /// Whether the heap formed, so a row passed to [`Self::push`] may be
    /// rewound once pushed: the heap copies the rows it keeps.
    pub(super) fn heap_formed(&self) -> bool {
        self.heap.is_some()
    }

    /// Takes the next row in arrival order. Before the heap forms, `row`
    /// must stay valid until [`Self::finish`].
    pub(super) fn push(
        &mut self,
        row: &'a [u8],
        compare: &mut RowCompare<'_>,
    ) -> Result<(), SqlError> {
        let first = *self.first.get_or_insert(row);
        let model = self
            .model
            .get_or_insert_with(|| self.tuples.model(Some(first), Some(self.bound)));
        let (_, chunk) = self
            .tuples
            .push_charged(self.source, model, Some(first), row)?;
        let bounded = model.bounded();
        let Some(heap) = self.heap.as_mut() else {
            self.early.push(row).map_err(|_| arena_full())?;
            if bounded {
                self.form_heap(compare)?;
            }
            return Ok(());
        };
        // `bounded_heap_charge` for a row past the heap's forming.
        heap.charge += chunk;
        let Heap {
            order,
            count,
            entries,
            charges,
            rows,
            charge,
        } = heap;
        let incoming = u32::MAX;
        let mut by_id = |left: u32, right: u32| {
            let at = |id: u32| {
                if id == incoming {
                    row
                } else {
                    rows.row(entries, id)
                }
            };
            compare(at(left), at(right))
        };
        match pg_qsort::bounded_heap_offer(&order[..*count], self.bound, incoming, &mut by_id)? {
            HeapOffer::Insert => unreachable!("a formed heap is full"),
            HeapOffer::Discard => *charge -= chunk,
            HeapOffer::ReplaceTop(top) => {
                *charge -= charges[top as usize];
                charges[top as usize] = chunk;
                rows.store(entries, top, row)?;
                let mut by_id = |left: u32, right: u32| {
                    compare(rows.row(entries, left), rows.row(entries, right))
                };
                pg_qsort::bounded_heap_replace_top(&mut order[..*count], top, &mut by_id)?;
            }
        }
        Ok(())
    }

    /// `make_bounded_heap` over the rows read so far, which it charges in
    /// full; a row replacing the heap's greatest leaves that one charged.
    fn form_heap(&mut self, compare: &mut RowCompare<'_>) -> Result<(), SqlError> {
        let early = self.early.borrowed_slice();
        let first = self.first.expect("a row arrived");
        let mut charge: usize = early
            .iter()
            .map(|row| self.tuples.chunk(self.source, first, row))
            .sum();
        let order = self
            .arena
            .alloc_persistent_slice_with(self.bound, |_| 0u32)
            .map_err(|_| arena_full())?;
        let mut count = 0usize;
        let mut by_at =
            |left: u32, right: u32| compare(early[left as usize], early[right as usize]);
        for at in 0..early.len() as u32 {
            match pg_qsort::bounded_heap_offer(&order[..count], self.bound, at, &mut by_at)? {
                HeapOffer::Insert => {
                    pg_qsort::bounded_heap_insert(order, &mut count, at, &mut by_at)?
                }
                HeapOffer::Discard => {
                    charge -= self.tuples.chunk(self.source, first, early[at as usize]);
                }
                HeapOffer::ReplaceTop(_) => {
                    pg_qsort::bounded_heap_replace_top(&mut order[..count], at, &mut by_at)?;
                }
            }
        }
        // The kept rows move to the leases, entry ids by heap position. The
        // heap's arrays are in the arena's tail: the caller rewinds its
        // front after each row.
        let pool = self.source.storage.plan_build();
        let mut rows = HeapRows {
            bytes: pool.lease(0).map_err(sort_full)?,
            spare: pool.lease(0).map_err(sort_full)?,
            garbage: 0,
        };
        let entries = self
            .arena
            .alloc_persistent_slice_with(self.bound, |_| (0u32, 0u32))
            .map_err(|_| arena_full())?;
        let charges = self
            .arena
            .alloc_persistent_slice_with(self.bound, |_| 0usize)
            .map_err(|_| arena_full())?;
        for (position, at) in order.iter_mut().enumerate().take(count) {
            let row = early[*at as usize];
            rows.store(entries, position as u32, row)?;
            charges[position] = self.tuples.chunk(self.source, first, row);
            *at = position as u32;
        }
        rows.garbage = 0;
        self.heap = Some(Heap {
            order,
            count,
            entries,
            charges,
            rows,
            charge,
        });
        Ok(())
    }

    /// The sorted rows the limit returns, in the arena, and what the sort
    /// reports.
    pub(super) fn finish(
        mut self,
        compare: &mut RowCompare<'_>,
    ) -> Result<(&'a mut [&'a [u8]], SortSpace), SqlError> {
        let Some(heap) = self.heap.take() else {
            let rows = self.early.into_mut_slice().map_err(|_| arena_full())?;
            let (kept, space) = self.tuples.sort_rows(
                self.source,
                self.arena,
                rows,
                Some(self.bound),
                &mut |left, right| compare(left, right),
            )?;
            return Ok((&mut rows[..kept], space));
        };
        let Heap {
            order,
            count,
            entries,
            rows,
            charge,
            ..
        } = heap;
        let order = &mut order[..count];
        pg_qsort::sort_bounded_heap(order, &mut |left, right| {
            compare(rows.row(entries, left), rows.row(entries, right))
        })?;
        let mut sorted = ArenaList::new_persistent(self.arena);
        for &id in order.iter() {
            let row = self
                .arena
                .alloc_persistent_slice_with(entries[id as usize].1 as usize, |_| 0u8)
                .map_err(|_| arena_full())?;
            row.copy_from_slice(rows.row(entries, id));
            sorted.push(&*row).map_err(|_| arena_full())?;
        }
        let model = self.model.take().expect("rows arrived");
        Ok((
            sorted.into_mut_slice().map_err(|_| arena_full())?,
            SortSpace::from_postgres(model.space(Some(charge)), true),
        ))
    }
}
