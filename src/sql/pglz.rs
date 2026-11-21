//! The size PostgreSQL's `pglz_compress` gives a value under its default
//! strategy, which TOAST uses to compress a wide column. Only the length is
//! computed: the history lookup and the tag and literal layout are ported
//! exactly, the output bytes are not written.
//! https://github.com/postgres/postgres/blob/REL_18_6/src/common/pg_lzcompress.c

/// `PGLZ_MAX_HISTORY_LISTS`, `PGLZ_HISTORY_SIZE` and `PGLZ_MAX_MATCH`.
const MAX_HISTORY_LISTS: usize = 8192;
const HISTORY_SIZE: usize = 4096;
const MAX_MATCH: usize = 273;

/// `strategy_default_data`: inputs under 32 bytes are not compressed, the
/// result must save 25%, compression gives up after 1024 output bytes
/// without a match, and a lookup stops at a 128-byte match, accepting
/// 10% less at each further entry.
const MIN_INPUT_SIZE: usize = 32;
const MIN_COMP_RATE: usize = 25;
const FIRST_SUCCESS_BY: usize = 1024;
const MATCH_SIZE_GOOD: usize = 128;
const MATCH_SIZE_DROP: usize = 10;

/// One `PGLZ_HistEntry`; index 0 is the invalid entry.
#[derive(Clone, Copy)]
struct Entry {
    next: u16,
    prev: u16,
    hindex: u16,
    pos: u32,
}

const INVALID: u16 = 0;

/// The history of one compression: `hist_start` and `hist_entries`.
struct History {
    start: [u16; MAX_HISTORY_LISTS],
    entries: [Entry; HISTORY_SIZE + 1],
    next: usize,
    recycle: bool,
    mask: usize,
}

/// `pglz_hist_idx`: the list for the next four input bytes, read as the
/// platform's signed `char` (the reference builds sign-extend them).
fn hist_idx(input: &[u8], at: usize, mask: usize) -> usize {
    let byte = |offset: usize| i32::from(input[at + offset] as i8);
    let index = if input.len() - at < 4 {
        byte(0)
    } else {
        (byte(0) << 6) ^ (byte(1) << 4) ^ (byte(2) << 2) ^ byte(3)
    };
    (index as usize) & mask
}

impl History {
    /// `pglz_hist_add`.
    fn add(&mut self, input: &[u8], at: usize) {
        let hindex = hist_idx(input, at, self.mask);
        let current = self.next;
        if self.recycle {
            let entry = self.entries[current];
            if entry.prev == INVALID {
                self.start[usize::from(entry.hindex)] = entry.next;
            } else {
                self.entries[usize::from(entry.prev)].next = entry.next;
            }
            if entry.next != INVALID {
                self.entries[usize::from(entry.next)].prev = entry.prev;
            }
        }
        let head = self.start[hindex];
        self.entries[current] = Entry {
            next: head,
            prev: INVALID,
            hindex: hindex as u16,
            pos: at as u32,
        };
        // The unused entry 0 takes the write when the list was empty.
        self.entries[usize::from(head)].prev = current as u16;
        self.start[hindex] = current as u16;
        self.next += 1;
        if self.next > HISTORY_SIZE {
            self.next = 1;
            self.recycle = true;
        }
    }

    /// `pglz_find_match`: the longest earlier match of the input at `at`
    /// worth a tag, as its length and offset.
    fn find_match(&self, input: &[u8], at: usize) -> Option<(usize, usize)> {
        let mut good_match = MATCH_SIZE_GOOD;
        let mut len = 0usize;
        let mut off = 0usize;
        let mut entry = self.start[hist_idx(input, at, self.mask)];
        while entry != INVALID {
            let hent = self.entries[usize::from(entry)];
            let hp = hent.pos as usize;
            let thisoff = at - hp;
            if thisoff >= 0x0fff {
                break;
            }
            let mut thislen = 0usize;
            if len >= 16 {
                if input[at..at + len] == input[hp..hp + len] {
                    thislen = len;
                    while at + thislen < input.len()
                        && input[at + thislen] == input[hp + thislen]
                        && thislen < MAX_MATCH
                    {
                        thislen += 1;
                    }
                }
            } else {
                while at + thislen < input.len()
                    && input[at + thislen] == input[hp + thislen]
                    && thislen < MAX_MATCH
                {
                    thislen += 1;
                }
            }
            if thislen > len {
                len = thislen;
                off = thisoff;
            }
            entry = hent.next;
            if entry != INVALID {
                if len >= good_match {
                    break;
                }
                good_match -= (good_match * MATCH_SIZE_DROP) / 100;
            }
        }
        (len > 2).then_some((len, off))
    }
}

/// The bytes `pglz_compress` writes for `source` under the default
/// strategy, or `None` when it fails (too short, or not compressible by
/// the strategy's rate).
pub(crate) fn compressed_size(source: &[u8]) -> Option<usize> {
    let slen = source.len();
    if slen < MIN_INPUT_SIZE || slen > i32::MAX as usize {
        return None;
    }
    let result_max = if slen > (i32::MAX as usize) / 100 {
        (slen / 100) * (100 - MIN_COMP_RATE)
    } else {
        (slen * (100 - MIN_COMP_RATE)) / 100
    };
    let hashsz = match slen {
        0..128 => 512,
        128..256 => 1024,
        256..512 => 2048,
        512..1024 => 4096,
        _ => 8192,
    };
    let mut history = History {
        start: [INVALID; MAX_HISTORY_LISTS],
        entries: [Entry {
            next: INVALID,
            prev: INVALID,
            hindex: 0,
            pos: 0,
        }; HISTORY_SIZE + 1],
        next: 1,
        recycle: false,
        mask: hashsz - 1,
    };
    // Output bytes, and the control bit the next item takes (`ctrl`): a new
    // control byte is reserved once eight items have used one.
    let mut out = 0usize;
    let mut ctrl: u8 = 0;
    let mut found_match = false;
    let mut dp = 0usize;
    while dp < slen {
        if out >= result_max {
            return None;
        }
        if !found_match && out >= FIRST_SUCCESS_BY {
            return None;
        }
        if ctrl == 0 {
            out += 1;
            ctrl = 1;
        }
        ctrl <<= 1;
        if let Some((len, _)) = history.find_match(source, dp) {
            out += if len > 17 { 3 } else { 2 };
            for _ in 0..len {
                history.add(source, dp);
                dp += 1;
            }
            found_match = true;
        } else {
            out += 1;
            history.add(source, dp);
            dp += 1;
        }
    }
    (out < result_max).then_some(out)
}

#[cfg(all(test, not(feature = "sql-fixture-suite")))]
mod tests {
    use super::*;

    #[test]
    fn short_and_incompressible_inputs_fail() {
        assert_eq!(compressed_size(&[b'a'; 31]), None);
        // A byte sequence without repeats never matches.
        let distinct: [u8; 200] = core::array::from_fn(|at| at as u8);
        assert_eq!(compressed_size(&distinct), None);
    }

    #[test]
    fn repeated_input_compresses_as_postgresql() {
        // `pg_column_size` of these stored as a table's text column on
        // PostgreSQL 18.6 is 44, 54 and 45: the 8-byte compressed header
        // and pglz's output.
        assert_eq!(compressed_size(&[b'x'; 3000]), Some(36));
        assert_eq!(compressed_size(&b"abcdefghij".repeat(300)), Some(46));
        assert_eq!(
            compressed_size(&"\u{e9}".repeat(1500).into_bytes()),
            Some(37)
        );
    }
}
