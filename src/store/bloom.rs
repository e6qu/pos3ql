//! A bloom filter over row identities, sized to one block.
//!
//! Its one job is to let a reader skip an SST that cannot hold a key without
//! reading that SST's index. When several SSTs might hold a key — the levels a
//! later stage will stack — checking a small filter first turns "read every
//! SST's index to find the key is not there" into "read one filter block and
//! move on". A filter that says *absent* is always right; one that says
//! *present* may be wrong, and the index read that follows settles it. There
//! are no false negatives, which is the only property correctness rests on.
//!
//! The bit array is a block payload, so the filter needs no memory of its own:
//! the writer fills a buffer that becomes the filter block, and the reader
//! queries the bytes it read back. The number of bits is the payload length in
//! bits, so a reader adapts to whatever size the writer chose without being
//! told.
//!
//! Membership uses double hashing — two hashes of the key, then `k` positions
//! `h1 + i·h2` — which gives `k` well-spread bits from one pass and avoids
//! computing `k` independent hashes. The two hashes come from a splitmix64
//! finalizer, which mixes a `u64` thoroughly enough that adjacent row
//! identities do not land on adjacent bits.

/// Bits set per key. `k = 7` is near the optimum for the ~10 bits per key the
/// default sizing gives, where the false-positive rate is under one percent.
const HASHES: usize = 7;

/// The fixed size of a filter block. One bit array serves the whole SST; at
/// 128 KiB it holds about a hundred thousand keys under one percent false
/// positives, and beyond that the rate rises gracefully — never to a false
/// negative, only to more index reads that find the key absent. A per-block or
/// sized filter is a later refinement; this is correct at every size.
pub(crate) const FILTER_BYTES: usize = 128 * 1024;

/// splitmix64's finalizing mix. Deterministic, and spreads a `u64` so the two
/// derived hashes are near-independent.
fn mix(mut x: u64) -> u64 {
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

/// The `k` bit positions a key touches, over `bits` total. `h2` is forced odd
/// so that stepping by it visits distinct residues rather than cycling early.
fn positions(key: u64, bits: usize) -> impl Iterator<Item = usize> {
    let h1 = mix(key);
    let h2 = mix(key ^ 0x9e37_79b9_7f4a_7c15) | 1;
    (0..HASHES).map(move |i| {
        let combined = h1.wrapping_add((i as u64).wrapping_mul(h2));
        (combined % bits as u64) as usize
    })
}

/// Sets the bits for `key` in a filter under construction.
pub(crate) fn insert(bits: &mut [u8], key: u64) {
    let total = bits.len() * 8;
    if total == 0 {
        return;
    }
    for position in positions(key, total) {
        bits[position / 8] |= 1 << (position % 8);
    }
}

/// Whether `key` might be present. `false` is certain; `true` is "read the
/// index to be sure". An empty filter admits everything, which is the safe
/// answer — it never claims a key is absent.
pub(crate) fn maybe_contains(bits: &[u8], key: u64) -> bool {
    let total = bits.len() * 8;
    if total == 0 {
        return true;
    }
    positions(key, total).all(|position| bits[position / 8] & (1 << (position % 8)) != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_that_was_inserted_is_never_reported_absent() {
        // The one invariant everything rests on: no false negatives.
        let mut bits = vec![0u8; FILTER_BYTES];
        let keys: Vec<u64> = (0..50_000u64).map(|i| i.wrapping_mul(2_654_435_761)).collect();
        for &k in &keys {
            insert(&mut bits, k);
        }
        for &k in &keys {
            assert!(maybe_contains(&bits, k), "inserted key {k} reported absent");
        }
    }

    #[test]
    fn absent_keys_are_rejected_far_more_often_than_not() {
        // A filter is only useful if "absent" is the common answer for keys that
        // were never inserted. With ~50k keys in 128 KiB the false-positive rate
        // should be small; this asserts a loose bound so it is not brittle, but
        // one tight enough that a broken hash (say every key on the same bits)
        // would blow it.
        let mut bits = vec![0u8; FILTER_BYTES];
        for i in 0..50_000u64 {
            insert(&mut bits, i.wrapping_mul(2_654_435_761));
        }
        let mut false_positives = 0;
        let trials = 100_000u64;
        for i in 0..trials {
            // Keys drawn from a disjoint range so none were inserted.
            let probe = (i | (1 << 40)).wrapping_mul(0x100_0001);
            if maybe_contains(&bits, probe) {
                false_positives += 1;
            }
        }
        let rate = false_positives as f64 / trials as f64;
        assert!(rate < 0.05, "false-positive rate {rate} is too high — check the hashing");
    }

    #[test]
    fn an_empty_filter_admits_everything() {
        // A zero-length filter must never claim absence, or a reader would skip
        // an SST that does hold the key.
        assert!(maybe_contains(&[], 1));
        assert!(maybe_contains(&[], u64::MAX));
    }

    #[test]
    fn a_fresh_filter_rejects_before_anything_is_inserted() {
        let bits = vec![0u8; 1024];
        assert!(!maybe_contains(&bits, 42), "an all-zero filter should reject");
    }

    #[test]
    fn distinct_keys_touch_distinct_bit_sets() {
        // Adjacent identities must not collapse onto the same bits, or the
        // filter would be far weaker than its size suggests.
        let a: Vec<_> = positions(1000, 1 << 20).collect();
        let b: Vec<_> = positions(1001, 1 << 20).collect();
        assert_ne!(a, b, "consecutive keys produced identical positions");
        assert_eq!(a.len(), HASHES);
    }

    #[test]
    fn a_single_byte_filter_still_works_correctly() {
        // Small filters are useless as filters but must not be wrong: an
        // inserted key is still never reported absent.
        let mut bits = vec![0u8; 1];
        insert(&mut bits, 7);
        assert!(maybe_contains(&bits, 7));
    }
}
