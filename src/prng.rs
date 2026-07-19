//! PCG32 (XSH-RR 64/32): the deterministic PRNG behind simulation testing
//! and any randomized runtime decision, so that every run reproduces
//! exactly from its seed on every platform.
//!
//! Algorithm from "PCG: A Family of Simple Fast Space-Efficient
//! Statistically Good Algorithms for Random Number Generation"
//! (M.E. O'Neill, <https://www.pcg-random.org/paper.html>), matching the
//! reference implementation `pcg_basic.c` in imneme/pcg-c-basic at commit
//! bc39cd76ac3d541e618606bcc6e1e5ba5e5e6aa3.

#[derive(Debug, Clone)]
pub struct Pcg32 {
    state: u64,
    inc: u64,
}

const MULTIPLIER: u64 = 6364136223846793005;

impl Pcg32 {
    /// Seeds from an initial state and a stream id, mirroring
    /// `pcg32_srandom_r`: two generator steps around the seed injection so
    /// that similar seeds do not yield similar first outputs.
    pub fn new(seed: u64, stream: u64) -> Self {
        let mut rng = Self {
            state: 0,
            inc: (stream << 1) | 1,
        };
        rng.next_u32();
        rng.state = rng.state.wrapping_add(seed);
        rng.next_u32();
        rng
    }

    pub fn next_u32(&mut self) -> u32 {
        let old = self.state;
        self.state = old.wrapping_mul(MULTIPLIER).wrapping_add(self.inc);
        let xorshifted = (((old >> 18) ^ old) >> 27) as u32;
        let rot = (old >> 59) as u32;
        xorshifted.rotate_right(rot)
    }

    pub fn next_u64(&mut self) -> u64 {
        (u64::from(self.next_u32()) << 32) | u64::from(self.next_u32())
    }

    /// Uniform value in `[0, bound)` without modulo bias, mirroring
    /// `pcg32_boundedrand_r`'s rejection scheme. Panics if `bound` is zero.
    pub fn next_bounded(&mut self, bound: u32) -> u32 {
        assert!(bound > 0, "next_bounded requires a non-zero bound");
        let threshold = bound.wrapping_neg() % bound;
        loop {
            let r = self.next_u32();
            if r >= threshold {
                return r % bound;
            }
        }
    }

    /// Uniform value in `[low, high]` (inclusive).
    pub fn next_range_inclusive(&mut self, low: u32, high: u32) -> u32 {
        assert!(low <= high, "empty range: {low}..={high}");
        let span = high - low;
        if span == u32::MAX {
            return self.next_u32();
        }
        low + self.next_bounded(span + 1)
    }

    /// True with probability `numerator / denominator`.
    pub fn chance(&mut self, numerator: u32, denominator: u32) -> bool {
        self.next_bounded(denominator) < numerator
    }

    pub fn fill_bytes(&mut self, dest: &mut [u8]) {
        for chunk in dest.chunks_mut(4) {
            let bytes = self.next_u32().to_le_bytes();
            chunk.copy_from_slice(&bytes[..chunk.len()]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Expected outputs generated from a transcription of `pcg_basic.c`
    /// (imneme/pcg-c-basic @ bc39cd76ac3d541e618606bcc6e1e5ba5e5e6aa3,
    /// sha256 of pcg_basic.c:
    /// b6582a071a8a090a293621523c063d125a532d772a2a1eb7d60b3e695fe47746).
    /// The (42, 54) sequence also matches the demo output published for
    /// `pcg32-demo -r` by the PCG project.
    #[test]
    fn matches_reference_vectors() {
        let cases: [(u64, u64, [u32; 6]); 3] = [
            (
                42,
                54,
                [
                    0xa15c02b7, 0x7b47f409, 0xba1d3330, 0x83d2f293, 0xbfa4784b, 0xcbed606e,
                ],
            ),
            (
                0,
                0,
                [
                    0xe4c14788, 0x379c6516, 0x5c4ab3bb, 0x601d23e0, 0x1c382b8c, 0xd1faab16,
                ],
            ),
            (
                0xdead_beef,
                0xcafe_f00d,
                [
                    0xa6971fe5, 0x6ccc9066, 0xd6ca1161, 0x80a872b6, 0xa0beadeb, 0xa4e34807,
                ],
            ),
        ];
        for (seed, stream, expected) in cases {
            let mut rng = Pcg32::new(seed, stream);
            let got: [u32; 6] = core::array::from_fn(|_| rng.next_u32());
            assert_eq!(got, expected, "seed={seed:#x} stream={stream:#x}");
        }
    }

    #[test]
    fn distinct_streams_diverge() {
        let mut a = Pcg32::new(1, 1);
        let mut b = Pcg32::new(1, 2);
        let same = (0..64).filter(|_| a.next_u32() == b.next_u32()).count();
        assert!(same < 4, "streams should be effectively independent");
    }

    #[test]
    fn bounded_is_in_range_and_covers() {
        let mut rng = Pcg32::new(7, 7);
        let mut seen = [false; 10];
        for _ in 0..1000 {
            let v = rng.next_bounded(10);
            assert!(v < 10);
            seen[v as usize] = true;
        }
        assert!(seen.iter().all(|s| *s), "all buckets hit in 1000 draws");
        for _ in 0..100 {
            let v = rng.next_range_inclusive(5, 8);
            assert!((5..=8).contains(&v));
        }
    }

    #[test]
    fn generation_does_not_allocate() {
        let mut rng = Pcg32::new(3, 3);
        crate::mem::guard::forbid_alloc(|| {
            let mut buffer = [0u8; 64];
            rng.fill_bytes(&mut buffer);
            let _ = rng.next_u64();
            let _ = rng.next_bounded(17);
        });
    }
}
