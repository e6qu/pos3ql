//! The pages a btree build writes (`_bt_load` in `nbtsort.c`): leaf tuples
//! arrive in key order, and each level fills pages until the next tuple does
//! not fit, or the page's free space falls under its fill reserve while it
//! holds two tuples. The page's last tuple then moves to a new page; on a
//! leaf the truncated pivot between it and the tuple before it
//! (`_bt_truncate`) becomes the old page's high key and the new page's low
//! key, which the parent level receives when the new page closes. An inner
//! page's first tuple is stored as a key-less 8-byte "minus infinity" tuple.
//! When the input ends each level's open page links into the level above
//! (`_bt_uppershutdown`), the top page becoming the root, and a metapage
//! precedes them. `pg_class.relpages` of a freshly built btree and the
//! planner's page and height inputs follow this layout.

use crate::sql::eval::{SqlError, sqlstate};
use crate::sql_err;

/// `PageGetFreeSpace` of an empty btree page: 8 kB less the page header,
/// the btree special space, the reserved high key line pointer and the line
/// pointer of the tuple being added.
const EMPTY_PAGE_FREE: usize = 8192 - 24 - 16 - 4 - 4;
const LINE_POINTER: usize = 4;
/// `sizeof(IndexTupleData)`: a key-less pivot.
const MINUS_INFINITY: usize = 8;
/// `MAXALIGN(sizeof(ItemPointerData))`: a leaf keeps room for a heap TID its
/// high key may need.
const HEAP_TID: usize = 8;
/// `BLCKSZ * (100 - BTREE_NONLEAF_FILLFACTOR) / 100`.
const INNER_RESERVE: usize = 8192 * 30 / 100;
/// More levels than any btree of 2^64 tuples has.
const MAX_LEVELS: usize = 24;
/// `BTREE_DEFAULT_FILLFACTOR`.
pub(crate) const DEFAULT_FILLFACTOR: u8 = 90;

/// The space a leaf page keeps free (`BTGetTargetPageFreeSpace`) at the
/// index's `fillfactor`.
pub(crate) const fn leaf_reserve(fillfactor: u8) -> usize {
    8192 * (100 - fillfactor as usize) / 100
}

/// A built btree's pages (the metapage included), the level of its root
/// (`btm_level`, 0 for a root leaf) and its leaf pages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BtreeLayout {
    pub(crate) pages: u64,
    pub(crate) height: u32,
    pub(crate) leaves: u64,
}

impl BtreeLayout {
    /// An index without tuples: its metapage alone.
    pub(crate) const EMPTY: Self = Self {
        pages: 1,
        height: 0,
        leaves: 0,
    };
}

#[derive(Clone, Copy)]
struct Level {
    /// `PageGetFreeSpace` of the open page.
    free: usize,
    /// Data tuples on the open page.
    tuples: u32,
    /// The open page's last tuple: its size, its truncatable posting list
    /// bytes, and (on a leaf) the pivot separating it from the tuple before.
    last_size: usize,
    last_extra: usize,
    last_pivot: usize,
    /// The open page's low key, which its parent receives once it closes.
    low_key: usize,
    pages: u64,
}

impl Level {
    const NEW: Self = Self {
        free: EMPTY_PAGE_FREE,
        tuples: 0,
        last_size: 0,
        last_extra: 0,
        last_pivot: 0,
        low_key: MINUS_INFINITY,
        pages: 1,
    };
}

/// A btree build fed its leaf tuples in key order.
pub(crate) struct BtreeBuild {
    levels: [Level; MAX_LEVELS],
    depth: usize,
    leaf_reserve: usize,
}

impl BtreeBuild {
    pub(crate) const fn new(fillfactor: u8) -> Self {
        Self {
            levels: [Level::NEW; MAX_LEVELS],
            depth: 0,
            leaf_reserve: leaf_reserve(fillfactor),
        }
    }

    /// Adds the next leaf tuple: its `MAXALIGN`ed size, the bytes of its
    /// posting list a page split may truncate (`truncextra`), and the size of
    /// the pivot `_bt_truncate` forms between the previous tuple and it.
    pub(crate) fn add_leaf(
        &mut self,
        size: usize,
        truncatable: usize,
        pivot: usize,
    ) -> Result<(), SqlError> {
        self.add(0, size, truncatable, pivot)
    }

    fn add(
        &mut self,
        level: usize,
        size: usize,
        truncatable: usize,
        pivot: usize,
    ) -> Result<(), SqlError> {
        if level == self.depth {
            if level == MAX_LEVELS {
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "btree layout exceeds {} levels",
                    MAX_LEVELS
                ));
            }
            self.levels[level] = Level::NEW;
            self.depth += 1;
        }
        let leaf = level == 0;
        let reserve = if leaf {
            self.leaf_reserve
        } else {
            INNER_RESERVE
        };
        let state = self.levels[level];
        if state.tuples > 0
            && (state.free < size + if leaf { HEAP_TID } else { 0 }
                || (state.free + state.last_extra < reserve && state.tuples >= 2))
        {
            // The last tuple moves to a new page; the old page's high key is
            // the leaf pivot before it, or the inner tuple itself.
            let high_key = if leaf {
                state.last_pivot
            } else {
                state.last_size
            };
            self.add(level + 1, state.low_key, 0, 0)?;
            let moved = if leaf {
                state.last_size
            } else {
                MINUS_INFINITY
            };
            let state = &mut self.levels[level];
            state.low_key = high_key;
            state.pages += 1;
            state.free = EMPTY_PAGE_FREE.saturating_sub(moved + LINE_POINTER);
            state.tuples = 1;
        }
        let state = &mut self.levels[level];
        let stored = if !leaf && state.tuples == 0 {
            MINUS_INFINITY
        } else {
            size
        };
        state.free = state.free.saturating_sub(stored + LINE_POINTER);
        state.tuples += 1;
        state.last_size = size;
        state.last_extra = truncatable;
        state.last_pivot = pivot;
        Ok(())
    }

    /// Links each level's open page into the level above and returns the
    /// finished layout.
    pub(crate) fn finish(mut self) -> Result<BtreeLayout, SqlError> {
        if self.depth == 0 {
            return Ok(BtreeLayout::EMPTY);
        }
        let mut level = 0;
        while level + 1 < self.depth {
            let low_key = self.levels[level].low_key;
            self.add(level + 1, low_key, 0, 0)?;
            level += 1;
        }
        let pages = self.levels[..self.depth]
            .iter()
            .map(|level| level.pages)
            .sum::<u64>();
        Ok(BtreeLayout {
            pages: 1 + pages,
            height: (self.depth - 1) as u32,
            leaves: self.levels[0].pages,
        })
    }
}

/// `MAXALIGN(basetupsize + n * sizeof(ItemPointerData))`: a posting list
/// tuple holding `tids` heap TIDs after a key of `base` bytes.
pub(crate) const fn posting_size(base: usize, tids: usize) -> usize {
    (base + 6 * tids).next_multiple_of(8)
}

/// The largest posting list tuple a build forms: a tenth of a page less a
/// line pointer (`maxpostingsize` in `_bt_load`).
pub(crate) const MAX_POSTING_SIZE: usize = (8192 * 10 / 100) / 8 * 8 - LINE_POINTER;

#[cfg(all(test, not(feature = "sql-fixture-suite")))]
mod tests {
    use super::*;

    /// Builds `count` tuples of one `size`, each separated from the one
    /// before by a pivot of `pivot` bytes.
    fn uniform(count: usize, size: usize, pivot: usize, fillfactor: u8) -> BtreeLayout {
        let mut build = BtreeBuild::new(fillfactor);
        for _ in 0..count {
            build.add_leaf(size, 0, pivot).unwrap();
        }
        build.finish().unwrap()
    }

    /// Equal keys deduplicated into posting lists of at most
    /// `MAX_POSTING_SIZE`, `keys` runs of `per_key` tuples whose key tuple
    /// is `base` bytes.
    fn deduplicated(keys: usize, per_key: usize, base: usize) -> BtreeLayout {
        let mut build = BtreeBuild::new(DEFAULT_FILLFACTOR);
        let per_posting = (1..)
            .take_while(|tids| posting_size(base, *tids) <= MAX_POSTING_SIZE)
            .last()
            .unwrap();
        for _ in 0..keys {
            let mut remaining = per_key;
            let mut first = true;
            while remaining > 0 {
                let tids = remaining.min(per_posting);
                remaining -= tids;
                let (size, extra) = if tids == 1 {
                    (base, 0)
                } else {
                    (posting_size(base, tids), posting_size(base, tids) - base)
                };
                // A new key's pivot keeps the key; an equal one adds a TID.
                let pivot = if first { base } else { base + HEAP_TID };
                build.add_leaf(size, extra, pivot).unwrap();
                first = false;
            }
        }
        build.finish().unwrap()
    }

    // PostgreSQL 18.6, measured 2026-11-22: `generate_series` keys indexed
    // after loading (`pg_class.relpages`).
    #[test]
    fn matches_postgres_builds_of_distinct_keys() {
        assert_eq!(BtreeBuild::new(90).finish().unwrap(), BtreeLayout::EMPTY);
        // An int4 key: 10,000 rows take 30 pages, 50,000 take 139.
        assert_eq!(uniform(10_000, 16, 16, 90).pages, 30);
        assert_eq!(uniform(50_000, 16, 16, 90).pages, 139);
        assert_eq!(uniform(300_000, 16, 16, 90).pages, 825);
        assert_eq!(uniform(300_000, 16, 16, 90).height, 2);
        // `fillfactor = 50`.
        assert_eq!(uniform(50_000, 16, 16, 50).pages, 250);
        // An `md5` text key: a 33-byte short varlena.
        assert_eq!(uniform(10_000, 48, 48, 90).pages, 74);
        assert_eq!(uniform(50_000, 48, 48, 90).pages, 364);
    }

    #[test]
    fn matches_postgres_deduplicated_builds() {
        // A boolean key holding 25,000 rows of each value: 45 pages.
        assert_eq!(deduplicated(2, 25_000, 16).pages, 45);
    }
}
