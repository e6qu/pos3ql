//! In-place heap sorting with a fixed amount of work per turn.

use super::HeapRelocationEntry;

pub(super) struct RelocationSort {
    build_root: usize,
    heap_len: usize,
    sift: Option<usize>,
}

impl RelocationSort {
    pub(super) fn new(len: usize) -> Self {
        Self {
            build_root: len / 2,
            heap_len: len,
            sift: None,
        }
    }

    fn complete(&self) -> bool {
        self.build_root == 0 && self.heap_len <= 1 && self.sift.is_none()
    }

    /// Each unit starts one root, extracts one entry, or sifts one level.
    /// A sift level compares at most two keys and swaps at most one pair.
    pub(super) fn step(
        &mut self,
        entries: &mut [HeapRelocationEntry],
        remaining: &mut usize,
    ) -> bool {
        let key = |entry: &HeapRelocationEntry| (entry.source.offset, entry.source.len);
        while *remaining != 0 && !self.complete() {
            *remaining -= 1;
            if let Some(root) = self.sift {
                let Some(child) = root
                    .checked_mul(2)
                    .and_then(|index| index.checked_add(1))
                    .filter(|&index| index < self.heap_len)
                else {
                    self.sift = None;
                    continue;
                };
                let right = child + 1;
                let largest =
                    if right < self.heap_len && key(&entries[child]) < key(&entries[right]) {
                        right
                    } else {
                        child
                    };
                if key(&entries[root]) < key(&entries[largest]) {
                    entries.swap(root, largest);
                    self.sift = Some(largest);
                } else {
                    self.sift = None;
                }
            } else if self.build_root != 0 {
                self.build_root -= 1;
                self.sift = Some(self.build_root);
            } else {
                self.heap_len -= 1;
                entries.swap(0, self.heap_len);
                self.sift = Some(0);
            }
        }
        self.complete()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::RowLoc;

    #[test]
    fn heap_plan_sort_small_turns_preserve_order_aliases_and_entry_identity() {
        let empty = HeapRelocationEntry {
            source: RowLoc::test(0, 0),
            destination: 0,
        };
        crate::mem::guard::forbid_alloc(|| {
            for len in [0usize, 1, 2, 3, 15, 16, 17, 63, 64, 65] {
                for pattern in 0..4 {
                    let mut entries = [empty; 65];
                    for (index, entry) in entries.iter_mut().take(len).enumerate() {
                        let rank = match pattern {
                            0 => len - index,
                            1 => index,
                            2 => (index * 17) % len.max(1),
                            _ => index % 3,
                        };
                        *entry = HeapRelocationEntry {
                            source: RowLoc::test((rank * 4) as u32, (rank % 3) as u32),
                            destination: index as u32,
                        };
                    }
                    let original = entries;
                    let mut expected = entries;
                    expected[..len]
                        .sort_unstable_by_key(|entry| (entry.source.offset, entry.source.len));
                    let mut sort = RelocationSort::new(len);
                    let bound = len.saturating_mul(2 * usize::BITS as usize + 5).max(1);
                    let mut done = false;
                    for turn in 0..bound {
                        let mut remaining = 1;
                        done = sort.step(&mut entries[..len], &mut remaining);
                        if turn == 0 && len > 2 {
                            assert!(!done, "one turn performed the complete sort");
                        }
                        if done {
                            break;
                        }
                    }
                    assert!(done, "bounded sorting did not finish");
                    let mut seen = 0u128;
                    for (entry, expected) in entries[..len].iter().zip(&expected[..len]) {
                        assert_eq!(
                            (entry.source.offset, entry.source.len),
                            (expected.source.offset, expected.source.len)
                        );
                        let index = entry.destination as usize;
                        assert!(index < len);
                        assert_eq!(entry.source, original[index].source);
                        assert_eq!(seen & (1u128 << index), 0);
                        seen |= 1u128 << index;
                    }
                    assert_eq!(seen, (1u128 << len) - 1);
                }
            }
        });
    }
}
