//! MD5 per RFC 1321, for the SQL `md5()` function. Implemented here because
//! the dependency policy admits no crypto crates; MD5 is a fixed, fully
//! specified algorithm validated against the RFC 1321 test vectors below.
//! (MD5 is used only as a non-cryptographic content digest for `md5()`; it is
//! never used for security in this codebase.)

/// Per-operation constants: `floor(2^32 * abs(sin(i + 1)))` (RFC 1321).
const T: [u32; 64] = [
    0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613,
    0xfd469501, 0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193,
    0xa679438e, 0x49b40821, 0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d,
    0x02441453, 0xd8a1e681, 0xe7d3fbc8, 0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed,
    0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a, 0xfffa3942, 0x8771f681, 0x6d9d6122,
    0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70, 0x289b7ec6, 0xeaa127fa,
    0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665, 0xf4292244,
    0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
    0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb,
    0xeb86d391,
];

/// Per-round left-rotate amounts (each of the four rounds repeats its four
/// shifts four times).
const S: [u32; 16] = [
    7, 12, 17, 22, // round 1
    5, 9, 14, 20, // round 2
    4, 11, 16, 23, // round 3
    6, 10, 15, 21, // round 4
];

fn shift(i: usize) -> u32 {
    // Round r = i / 16; within a round the shift cycles every 4 operations.
    S[(i / 16) * 4 + (i % 4)]
}

/// Computes the 16-byte MD5 digest of `msg`.
pub fn digest(msg: &[u8]) -> [u8; 16] {
    let mut a0: u32 = 0x67452301;
    let mut b0: u32 = 0xefcdab89;
    let mut c0: u32 = 0x98badcfe;
    let mut d0: u32 = 0x10325476;

    // Process each 64-byte block, including the padded tail. `block_at`
    // returns the 64-byte block starting at bit-padding position `base`.
    let bit_len = (msg.len() as u64).wrapping_mul(8);
    // Total padded length: message + 0x80 + zeros to 56 mod 64 + 8-byte length.
    let padded_len = {
        let with_one = msg.len() + 1;
        let pad_zeros = (56usize.wrapping_sub(with_one % 64)) % 64;
        with_one + pad_zeros + 8
    };

    let byte_at = |i: usize| -> u8 {
        if i < msg.len() {
            msg[i]
        } else if i == msg.len() {
            0x80
        } else if i >= padded_len - 8 {
            // Little-endian 64-bit bit length.
            let k = i - (padded_len - 8);
            (bit_len >> (8 * k)) as u8
        } else {
            0
        }
    };

    let mut off = 0usize;
    while off < padded_len {
        let mut m = [0u32; 16];
        for (j, word) in m.iter_mut().enumerate() {
            let b0 = byte_at(off + j * 4) as u32;
            let b1 = byte_at(off + j * 4 + 1) as u32;
            let b2 = byte_at(off + j * 4 + 2) as u32;
            let b3 = byte_at(off + j * 4 + 3) as u32;
            *word = b0 | (b1 << 8) | (b2 << 16) | (b3 << 24);
        }
        let (mut a, mut b, mut c, mut d) = (a0, b0, c0, d0);
        // `i` drives the round function, the message-word index, the constant
        // T[i], and the rotate amount — not a plain array walk.
        #[allow(clippy::needless_range_loop)]
        for i in 0..64 {
            let (f, g) = if i < 16 {
                ((b & c) | (!b & d), i)
            } else if i < 32 {
                ((d & b) | (!d & c), (5 * i + 1) % 16)
            } else if i < 48 {
                (b ^ c ^ d, (3 * i + 5) % 16)
            } else {
                (c ^ (b | !d), (7 * i) % 16)
            };
            let f = f
                .wrapping_add(a)
                .wrapping_add(T[i])
                .wrapping_add(m[g]);
            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(f.rotate_left(shift(i)));
        }
        a0 = a0.wrapping_add(a);
        b0 = b0.wrapping_add(b);
        c0 = c0.wrapping_add(c);
        d0 = d0.wrapping_add(d);
        off += 64;
    }

    let mut out = [0u8; 16];
    for (i, word) in [a0, b0, c0, d0].iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_le_bytes());
    }
    out
}

/// Writes the 32-character lowercase hex of an MD5 digest into `out`.
pub fn hex(d: &[u8; 16], out: &mut [u8; 32]) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for (i, byte) in d.iter().enumerate() {
        out[i * 2] = HEX[(byte >> 4) as usize];
        out[i * 2 + 1] = HEX[(byte & 0xf) as usize];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn md5_hex(s: &str) -> String {
        let d = digest(s.as_bytes());
        let mut h = [0u8; 32];
        hex(&d, &mut h);
        String::from_utf8(h.to_vec()).unwrap()
    }

    #[test]
    fn rfc1321_vectors() {
        // The suite from RFC 1321 appendix A.5.
        assert_eq!(md5_hex(""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(md5_hex("a"), "0cc175b9c0f1b6a831c399e269772661");
        assert_eq!(md5_hex("abc"), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(md5_hex("message digest"), "f96b697d7cb7938d525a2f31aaf161d0");
        assert_eq!(
            md5_hex("abcdefghijklmnopqrstuvwxyz"),
            "c3fcd3d76192e4007dfb496cca67e13b"
        );
        assert_eq!(
            md5_hex("ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789"),
            "d174ab98d277d9f5a5611c2c9f419d9f"
        );
        assert_eq!(
            md5_hex("12345678901234567890123456789012345678901234567890123456789012345678901234567890"),
            "57edf4a22be3c955ac49da2e2107b67a"
        );
    }
}
