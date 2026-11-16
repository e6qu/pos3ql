//! The heap pages PostgreSQL would store a table's rows in: each row as a
//! heap tuple (a 23-byte header, a null bitmap when a value is NULL, the
//! values at their type alignment, a short varlena unaligned) appended to the
//! last 8 kB page while it fits, with a four-byte line pointer per tuple and
//! at most 291 tuples per page (`MaxHeapTuplesPerPage`). `pg_class.relpages`,
//! the planner's page counts and a bitmap heap scan's `Heap Blocks` follow
//! this layout.

use crate::sql::types::{ColType, Datum};

const BLOCK_SIZE: usize = 8192;
const PAGE_HEADER: usize = 24;
const LINE_POINTER: usize = 4;
const MAX_TUPLES_PER_PAGE: usize = 291;
const MAXIMUM_ALIGNMENT: usize = 8;

fn alignment(code: u8) -> usize {
    match code {
        b'd' => 8,
        b'i' => 4,
        b's' => 2,
        _ => 1,
    }
}

/// The length of the heap tuple holding `values` of types `schema`.
pub(crate) fn tuple_length(values: &[Datum<'_>], schema: &[ColType]) -> usize {
    let has_nulls = values.iter().any(Datum::is_null);
    let header = 23
        + if has_nulls {
            values.len().div_ceil(8)
        } else {
            0
        };
    let mut data = 0usize;
    for (value, &ctype) in values.iter().zip(schema) {
        if value.is_null() {
            continue;
        }
        let layout = ctype.physical_layout();
        if layout.length > 0 {
            data = data.next_multiple_of(alignment(layout.alignment)) + layout.length as usize;
            continue;
        }
        let raw = super::scalar::raw_value_length(value);
        if raw < 0x7f {
            // A short varlena: one header byte, no alignment.
            data += raw + 1;
        } else {
            data = data.next_multiple_of(alignment(layout.alignment)) + raw + 4;
        }
    }
    header.next_multiple_of(MAXIMUM_ALIGNMENT) + data
}

/// Rows packed into heap pages in physical order.
#[derive(Clone, Copy, Debug)]
pub(crate) struct HeapPacking {
    pages: u64,
    free: usize,
    tuples: usize,
}

impl Default for HeapPacking {
    fn default() -> Self {
        Self::new()
    }
}

impl HeapPacking {
    pub(crate) const fn new() -> Self {
        Self {
            pages: 0,
            free: 0,
            tuples: MAX_TUPLES_PER_PAGE,
        }
    }

    /// Places a tuple of `length` bytes; returns its zero-based page.
    pub(crate) fn add(&mut self, length: usize) -> u64 {
        let needed = length.next_multiple_of(MAXIMUM_ALIGNMENT) + LINE_POINTER;
        if self.pages == 0 || self.tuples == MAX_TUPLES_PER_PAGE || needed > self.free {
            self.pages += 1;
            self.free = BLOCK_SIZE - PAGE_HEADER;
            self.tuples = 0;
        }
        self.free = self.free.saturating_sub(needed);
        self.tuples += 1;
        self.pages - 1
    }

    pub(crate) const fn pages(&self) -> u64 {
        self.pages
    }
}
