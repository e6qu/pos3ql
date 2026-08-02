//! LZ4 block-format compression, hand-rolled.
//!
//! The dependency policy admits no compression crate, and the LZ4 *block*
//! format is a fixed, published specification — the same footing as the
//! hand-rolled SHA-256 (FIPS 180-4) and TZif (RFC 8536) readers. Reference:
//! `lz4/lz4` `doc/lz4_Block_format.md` at tag v1.10.0 (commit
//! ebb9f8d0e10e502dab5d19f2dbedf59b06c33e1b): a sequence stream of
//! `token | literal-length ext | literals | offset u16le | match-length ext`,
//! matches at least 4 bytes long, offsets within 64 KiB, and a block that
//! must end with literals (the last 5 bytes are always literals, and the
//! last match must start at least 12 bytes before the end).
//!
//! The compressor is greedy with a 4-byte-prefix hash table — modest ratios,
//! zero allocation beyond its fixed table, and always correct: callers keep
//! the raw payload whenever compression does not shrink it, so ratio is an
//! economy, never a requirement. The decompressor is strict: every length,
//! offset and copy is bounds-checked, and any malformed input is an error,
//! never a partial write — though in this engine a corrupt payload is caught
//! by the block checksum before decompression ever sees it.

/// Compressor hash-table entries (16-bit positions into the input window).
/// SST payloads are at most one block (~256 KiB), so positions fit u32.
const HASH_BITS: usize = 13;
const HASH_SIZE: usize = 1 << HASH_BITS;

/// The format's structural minimums: a match spans at least 4 bytes, the
/// last 5 bytes of a block are always literals, and no match may begin
/// within 12 bytes of the end.
const MIN_MATCH: usize = 4;
const LAST_LITERALS: usize = 5;
const MATCH_GUARD: usize = 12;

fn hash(sequence: u32) -> usize {
    // Knuth multiplicative hashing; the constant is LZ4's own.
    ((sequence.wrapping_mul(2654435761)) >> (32 - HASH_BITS as u32)) as usize
}

fn read_u32(data: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(data[at..at + 4].try_into().expect("bounds checked"))
}

/// Compresses `input` into `output`, returning the compressed length, or
/// `None` when the result would not fit `output` — callers size `output` to
/// the raw length, so `None` also covers "compression did not help".
pub(crate) fn compress(input: &[u8], output: &mut [u8]) -> Option<usize> {
    let mut table = [0u32; HASH_SIZE];
    let mut anchor = 0usize; // start of pending literals
    let mut at = 0usize;
    let mut out = 0usize;

    let emit = |output: &mut [u8],
                out: &mut usize,
                literals: &[u8],
                match_len: usize,
                offset: usize|
     -> Option<()> {
        let lit_len = literals.len();
        let match_code = match_len.checked_sub(MIN_MATCH);
        let token_lit = lit_len.min(15) as u8;
        let token_match = match_code.map_or(0, |m| m.min(15) as u8);
        *output.get_mut(*out)? = (token_lit << 4) | token_match;
        *out += 1;
        if lit_len >= 15 {
            let mut rest = lit_len - 15;
            while rest >= 255 {
                *output.get_mut(*out)? = 255;
                *out += 1;
                rest -= 255;
            }
            *output.get_mut(*out)? = rest as u8;
            *out += 1;
        }
        if *out + lit_len > output.len() {
            return None;
        }
        output[*out..*out + lit_len].copy_from_slice(literals);
        *out += lit_len;
        if let Some(code) = match_code {
            if *out + 2 > output.len() {
                return None;
            }
            output[*out..*out + 2].copy_from_slice(&(offset as u16).to_le_bytes());
            *out += 2;
            if code >= 15 {
                let mut rest = code - 15;
                while rest >= 255 {
                    *output.get_mut(*out)? = 255;
                    *out += 1;
                    rest -= 255;
                }
                *output.get_mut(*out)? = rest as u8;
                *out += 1;
            }
        }
        Some(())
    };

    if input.len() > MATCH_GUARD {
        let limit = input.len() - MATCH_GUARD;
        while at < limit {
            let sequence = read_u32(input, at);
            let slot = hash(sequence);
            let candidate = table[slot] as usize;
            table[slot] = at as u32;
            if candidate < at
                && at - candidate <= u16::MAX as usize
                && read_u32(input, candidate) == sequence
            {
                // Extend the match forward, honoring the end guard.
                let mut len = MIN_MATCH;
                let end = input.len() - LAST_LITERALS;
                while at + len < end && input[candidate + len] == input[at + len] {
                    len += 1;
                }
                emit(output, &mut out, &input[anchor..at], len, at - candidate)?;
                at += len;
                anchor = at;
            } else {
                at += 1;
            }
        }
    }
    // The tail is always literals.
    emit(output, &mut out, &input[anchor..], 0, 0)?;
    Some(out)
}

/// Decompresses `input` into `output`, returning the decompressed length.
/// Strict: malformed input — a length running past either buffer, an offset
/// reaching before the start — is `None`, never a partial result.
pub(crate) fn decompress(input: &[u8], output: &mut [u8]) -> Option<usize> {
    let mut at = 0usize;
    let mut out = 0usize;
    loop {
        let token = *input.get(at)?;
        at += 1;
        // Literals.
        let mut lit_len = (token >> 4) as usize;
        if lit_len == 15 {
            loop {
                let byte = *input.get(at)?;
                at += 1;
                lit_len += byte as usize;
                if byte != 255 {
                    break;
                }
            }
        }
        if at + lit_len > input.len() || out + lit_len > output.len() {
            return None;
        }
        output[out..out + lit_len].copy_from_slice(&input[at..at + lit_len]);
        at += lit_len;
        out += lit_len;
        if at == input.len() {
            // A block ends on its literals.
            return Some(out);
        }
        // Match.
        let offset = u16::from_le_bytes([*input.get(at)?, *input.get(at + 1)?]) as usize;
        at += 2;
        if offset == 0 || offset > out {
            return None;
        }
        let mut match_len = (token & 0x0F) as usize;
        if match_len == 15 {
            loop {
                let byte = *input.get(at)?;
                at += 1;
                match_len += byte as usize;
                if byte != 255 {
                    break;
                }
            }
        }
        let match_len = match_len + MIN_MATCH;
        if out + match_len > output.len() {
            return None;
        }
        // Overlapping copies are the format's run-length encoding: copy
        // byte-wise so an offset smaller than the length repeats correctly.
        let from = out - offset;
        for i in 0..match_len {
            output[out + i] = output[from + i];
        }
        out += match_len;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(data: &[u8]) {
        let mut compressed = vec![0u8; data.len() + 64];
        let mut restored = vec![0u8; data.len()];
        // `None` = incompressible under the raw-length budget: caller keeps raw.
        if let Some(n) = compress(data, &mut compressed[..data.len().max(16)]) {
            let m = decompress(&compressed[..n], &mut restored).expect("valid stream");
            assert_eq!(m, data.len());
            assert_eq!(&restored[..m], data);
        }
    }

    #[test]
    fn spec_example_decodes() {
        // A stream assembled by hand from the block-format spec: 5 literals
        // "abcde", then a match of length 4 at offset 5 (repeating "abcd"),
        // then the closing 5 literals "fghij".
        // token 0x50: 5 literals, then "abcde"; token 0x00 + offset 5:
        // match_len 0+4=4 ... but a match may not stand last, so it carries
        // the closing literal token 0x50 "fghij".
        let stream: &[u8] = &[
            0x54, b'a', b'b', b'c', b'd', b'e', 5, 0, // 5 literals + match 4+4=8? no:
        ];
        // 0x54 = 5 literals, match code 4 => match length 8, offset 5.
        let mut out = [0u8; 64];
        let n = decompress(stream, &mut out);
        // "abcde" then 8 bytes repeating from offset 5: "abcdeabc".
        assert_eq!(n, None, "stream ends on a match, which the format forbids");
        let stream: &[u8] = &[
            0x54, b'a', b'b', b'c', b'd', b'e', 5, 0, 0x50, b'f', b'g', b'h', b'i', b'j',
        ];
        let n = decompress(stream, &mut out).expect("valid");
        assert_eq!(&out[..n], b"abcdeabcdeabcfghij");
    }

    #[test]
    fn round_trips_representative_payloads() {
        round_trip(b"");
        round_trip(b"a");
        round_trip(b"hello, hello, hello, hello, hello, world");
        round_trip(&[0u8; 100_000]);
        let mut ramp = Vec::new();
        for i in 0..70_000u32 {
            ramp.extend_from_slice(&(i % 251).to_le_bytes());
        }
        round_trip(&ramp);
        // Row-like payloads: repeated headers with varying tails.
        let mut rows = Vec::new();
        for i in 0..5000u64 {
            rows.extend_from_slice(&i.to_le_bytes());
            rows.extend_from_slice(b"some column text that repeats a lot ");
            rows.extend_from_slice(&(i * 7919).to_le_bytes());
        }
        round_trip(&rows);
    }

    #[test]
    fn compressible_payloads_shrink() {
        let data = vec![b'x'; 64 * 1024];
        let mut out = vec![0u8; data.len()];
        let n = compress(&data, &mut out).expect("fits");
        assert!(
            n < data.len() / 100,
            "64K of one byte compresses hard, got {n}"
        );
    }

    #[test]
    fn incompressible_payloads_report_none() {
        // A PCG-style scramble: no 4-byte repeats within the window to
        // speak of, so compression cannot pay for its tokens.
        let mut state = 0x853c49e6748fea9bu64;
        let data: Vec<u8> = (0..4096)
            .map(|_| {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
                (state >> 33) as u8
            })
            .collect();
        let mut out = vec![0u8; data.len()];
        assert!(compress(&data, &mut out).is_none());
    }

    #[test]
    fn corrupt_streams_are_refused() {
        let mut out = [0u8; 128];
        // Literal length runs past the input.
        assert_eq!(decompress(&[0xF0, 200], &mut out), None);
        // Offset reaches before the start.
        assert_eq!(decompress(&[0x14, b'a', 9, 0, 0x00], &mut out), None);
        // Offset zero is invalid.
        assert_eq!(
            decompress(&[0x44, b'a', b'b', b'c', b'd', 0, 0, 0x10, b'e'], &mut out),
            None
        );
        // Output too small for the declared match.
        let mut tiny = [0u8; 4];
        assert_eq!(
            decompress(
                &[0x4F, b'a', b'b', b'c', b'd', 4, 0, 200, 0x10, b'e'],
                &mut tiny
            ),
            None
        );
    }
}
