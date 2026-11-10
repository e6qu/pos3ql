//! A fixed-size byte buffer for socket I/O: bytes are appended at the end,
//! consumed from the front, and compacted in place. The capacity bounds the
//! largest protocol message the server will accept or produce.

use super::budget::{Budget, BudgetError};

pub struct FixedBuf {
    buffer: Box<[u8]>,
    start: usize,
    end: usize,
    /// Bytes at the end of `buffer` held by a second region that grows
    /// downward. Appends never reach it; socket buffers never use it.
    tail: usize,
}

impl FixedBuf {
    pub fn new(
        budget: &mut Budget,
        what: &'static str,
        capacity: usize,
    ) -> Result<Self, BudgetError> {
        budget.draw(capacity, what)?;
        Ok(Self {
            buffer: vec![0; capacity].into_boxed_slice(),
            start: 0,
            end: 0,
            tail: 0,
        })
    }

    /// Unread bytes.
    pub fn readable(&self) -> &[u8] {
        &self.buffer[self.start..self.end]
    }

    /// Marks `n` readable bytes as consumed.
    pub fn consume(&mut self, n: usize) {
        assert!(
            n <= self.end - self.start,
            "consuming more than is readable"
        );
        self.start += n;
        if self.start == self.end {
            self.start = 0;
            self.end = 0;
        }
    }

    /// Space to append into, after moving any unread bytes to the front.
    pub fn writable(&mut self) -> &mut [u8] {
        if self.start > 0 {
            self.buffer.copy_within(self.start..self.end, 0);
            self.end -= self.start;
            self.start = 0;
        }
        let limit = self.buffer.len() - self.tail;
        &mut self.buffer[self.end..limit]
    }

    /// Marks `n` bytes of the writable slice as filled.
    pub fn advance(&mut self, n: usize) {
        assert!(
            self.end + n <= self.buffer.len() - self.tail,
            "advancing past capacity"
        );
        self.end += n;
    }

    pub fn append(&mut self, bytes: &[u8]) -> bool {
        let space = self.writable();
        if bytes.len() > space.len() {
            return false;
        }
        space[..bytes.len()].copy_from_slice(bytes);
        self.advance(bytes.len());
        true
    }

    pub fn len(&self) -> usize {
        self.end - self.start
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn capacity(&self) -> usize {
        self.buffer.len()
    }

    pub fn clear(&mut self) {
        self.start = 0;
        self.end = 0;
    }

    /// Raw filled region, for length back-patching by message writers.
    pub fn filled_mut(&mut self) -> &mut [u8] {
        &mut self.buffer[..self.end]
    }

    /// Absolute offset where the next appended byte will land, stable until
    /// the next `writable()`/`consume()` compaction. Pairs with
    /// `truncate_to` to roll back a partially written message.
    pub fn mark(&self) -> usize {
        self.end
    }

    /// Claims `length` bytes below the downward region, returning their
    /// absolute offset, or `None` when they would overlap appended bytes.
    pub fn push_tail(&mut self, length: usize) -> Option<(usize, &mut [u8])> {
        let tail = self.tail.checked_add(length)?;
        if self.end.checked_add(tail)? > self.buffer.len() {
            return None;
        }
        self.tail = tail;
        let offset = self.buffer.len() - tail;
        Some((offset, &mut self.buffer[offset..offset + length]))
    }

    /// Bytes previously claimed by [`Self::push_tail`].
    pub fn tail_bytes(&self, offset: usize, length: usize) -> &[u8] {
        assert!(
            offset >= self.buffer.len() - self.tail && offset + length <= self.buffer.len(),
            "tail range outside the downward region"
        );
        &self.buffer[offset..offset + length]
    }

    /// Releases the downward region; appended bytes are unaffected.
    pub fn clear_tail(&mut self) {
        self.tail = 0;
    }

    /// Moves `range` of the appended bytes to `to` (an earlier offset).
    pub fn move_within(&mut self, range: core::ops::Range<usize>, to: usize) {
        assert!(range.end <= self.end && to <= range.start, "invalid move");
        self.buffer.copy_within(range, to);
    }

    pub fn truncate_to(&mut self, mark: usize) {
        assert!(
            mark >= self.start && mark <= self.end,
            "invalid truncate mark"
        );
        self.end = mark;
    }
}

/// `write!` support; errors when the buffer is full (caller decides how
/// loud that is).
impl core::fmt::Write for FixedBuf {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        if self.append(s.as_bytes()) {
            Ok(())
        } else {
            Err(core::fmt::Error)
        }
    }
}

#[cfg(all(test, not(all(miri, feature = "miri-ownership"))))]
mod tests {
    use super::*;

    #[test]
    fn append_consume_compact() {
        let mut budget = Budget::new(1024);
        let mut b = FixedBuf::new(&mut budget, "test", 8).unwrap();
        assert!(b.append(b"abcdef"));
        assert_eq!(b.readable(), b"abcdef");
        b.consume(4);
        assert_eq!(b.readable(), b"ef");
        // Only 2 bytes free at the tail, but compaction makes room for 6.
        assert!(b.append(b"ghijkl"));
        assert_eq!(b.readable(), b"efghijkl");
        assert!(!b.append(b"x"), "full buffer rejects appends");
        b.consume(8);
        assert!(b.is_empty());
        assert!(b.append(b"12345678"));
    }

    #[test]
    fn mark_and_truncate_roll_back() {
        let mut budget = Budget::new(1024);
        let mut b = FixedBuf::new(&mut budget, "test", 16).unwrap();
        b.append(b"keep");
        let mark = b.mark();
        b.append(b"discard");
        b.truncate_to(mark);
        assert_eq!(b.readable(), b"keep");
    }

    #[test]
    fn downward_region_and_appends_share_capacity_without_overlap() {
        let mut budget = Budget::new(1024);
        let mut b = FixedBuf::new(&mut budget, "test", 12).unwrap();
        assert!(b.append(b"head"));
        let (first, bytes) = b.push_tail(3).unwrap();
        bytes.copy_from_slice(b"one");
        assert_eq!(first, 9);
        let (second, bytes) = b.push_tail(5).unwrap();
        bytes.copy_from_slice(b"two!!");
        assert_eq!(second, 4);
        assert!(
            b.push_tail(1).is_none(),
            "the region stops at appended bytes"
        );
        assert!(!b.append(b"x"), "appends stop at the region");
        assert_eq!(b.tail_bytes(first, 3), b"one");
        assert_eq!(b.tail_bytes(second, 5), b"two!!");
        b.clear();
        assert_eq!(
            b.tail_bytes(first, 3),
            b"one",
            "clearing appends keeps the region"
        );
        assert!(b.append(b"four"));
        assert!(!b.append(b"x"));
        b.clear_tail();
        assert!(b.append(b"more"));
        assert_eq!(b.readable(), b"fourmore");
    }
}
