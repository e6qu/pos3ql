//! CRC-32C (Castagnoli), the storage checksum. Software table
//! implementation; the polynomial (reflected 0x82F63B78) and the check
//! value for "123456789" (0xE3069283) are per RFC 3720 appendix B.4.

const fn build_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u32;
        let mut j = 0;
        while j < 8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0x82F6_3B78
            } else {
                crc >> 1
            };
            j += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

static TABLE: [u32; 256] = build_table();

pub fn crc32c(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in bytes {
        crc = (crc >> 8) ^ TABLE[((crc ^ u32::from(b)) & 0xff) as usize];
    }
    !crc
}

/// Incremental form for checksumming a record in pieces.
pub struct Crc32c(u32);

impl Crc32c {
    pub fn new() -> Self {
        Self(!0)
    }

    pub fn update(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0 >> 8) ^ TABLE[((self.0 ^ u32::from(b)) & 0xff) as usize];
        }
    }

    pub fn finish(&self) -> u32 {
        !self.0
    }
}

impl Default for Crc32c {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3720_check_value() {
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
    }

    #[test]
    fn empty_and_incremental() {
        assert_eq!(crc32c(b""), 0);
        let mut inc = Crc32c::new();
        inc.update(b"1234");
        inc.update(b"56789");
        assert_eq!(inc.finish(), 0xE306_9283);
    }

    #[test]
    fn detects_bit_flips() {
        let mut data = *b"the quick brown fox";
        let clean = crc32c(&data);
        data[7] ^= 0x01;
        assert_ne!(crc32c(&data), clean);
    }
}
