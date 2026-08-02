//! PostgreSQL network address types: `inet`, `cidr`, `macaddr`, `macaddr8`.
//!
//! Parsing, canonical text output, and the value operations behind the network
//! operators and functions. An address is a [`NetAddr`] — a family tag (4 or
//! 6), a mask length in bits, and a 16-byte buffer holding the address in
//! network byte order (a v4 address in the first four bytes). `inet` and `cidr`
//! share the representation; they differ only in input validation (`cidr`
//! forbids bits set to the right of the mask) and output (`cidr` always prints
//! the mask length).

use core::fmt;

/// An IPv4 or IPv6 host/network address with a mask length, backing both
/// `inet` and `cidr`. `family` is 4 or 6; `bits` is the mask length
/// (`0..=32` for v4, `0..=128` for v6); `addr` holds the address bytes in
/// network order, a v4 address in `addr[0..4]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetAddr {
    pub family: u8,
    pub bits: u8,
    pub addr: [u8; 16],
}

impl NetAddr {
    /// The number of address bytes this family uses (4 or 16).
    pub fn addr_len(&self) -> usize {
        if self.family == 4 { 4 } else { 16 }
    }

    /// The maximum mask length for this family (32 or 128).
    pub fn max_bits(&self) -> u8 {
        if self.family == 4 { 32 } else { 128 }
    }

    /// A copy with every host bit (right of the mask) cleared — the network
    /// this address belongs to, as an `inet`→`cidr` cast produces.
    pub fn to_network(mut self) -> NetAddr {
        clear_host_bits(&mut self.addr, self.bits);
        self
    }

    /// A copy with the mask dropped (family-max bits), so text output omits it —
    /// the `host()` function's value.
    pub fn host_only(mut self) -> NetAddr {
        self.bits = self.max_bits();
        self
    }

    /// The broadcast address: every host bit set to one, mask preserved.
    pub fn broadcast(mut self) -> NetAddr {
        let full = (self.bits / 8) as usize;
        let rem = self.bits % 8;
        let len = self.addr_len();
        for (i, byte) in self.addr[..len].iter_mut().enumerate() {
            if i > full || (i == full && rem == 0) {
                *byte = 0xff;
            } else if i == full {
                *byte |= 0xffu8 >> rem;
            }
        }
        self
    }

    /// The netmask as an address (`255.255.255.0`), mask length family-max.
    pub fn netmask(self) -> NetAddr {
        let mut addr = [0u8; 16];
        set_prefix_ones(&mut addr, self.bits, self.addr_len());
        NetAddr {
            family: self.family,
            bits: self.max_bits(),
            addr,
        }
    }

    /// The hostmask (`0.0.0.255`) — the bitwise inverse of the netmask over the
    /// family width — with mask length family-max.
    pub fn hostmask(self) -> NetAddr {
        let mut addr = [0u8; 16];
        set_prefix_ones(&mut addr, self.bits, self.addr_len());
        let len = self.addr_len();
        for byte in addr[..len].iter_mut() {
            *byte = !*byte;
        }
        NetAddr {
            family: self.family,
            bits: self.max_bits(),
            addr,
        }
    }

    /// A copy with a new mask length; for a `cidr` the host bits beyond the new
    /// mask are cleared.
    pub fn with_masklen(mut self, bits: u8, clear_host: bool) -> NetAddr {
        self.bits = bits;
        if clear_host {
            clear_host_bits(&mut self.addr, bits);
        }
        self
    }

    /// Ordering key matching PostgreSQL's `network_cmp`: family, then address
    /// bytes, then mask length.
    pub fn cmp_key(&self) -> [u8; 18] {
        let mut key = [0u8; 18];
        key[0] = self.family;
        key[1..17].copy_from_slice(&self.addr);
        key[17] = self.bits;
        key
    }
}

/// Whether byte `b`'s low `keep` bits (from the most-significant end) are the
/// only ones set — i.e. the trailing `8 - keep` bits are zero.
fn high_bits_only(byte: u8, keep: u32) -> bool {
    if keep >= 8 {
        return true;
    }
    let mask = if keep == 0 { 0xffu8 } else { 0xffu8 >> keep };
    byte & mask == 0
}

/// True if every address bit at or beyond position `bits` is zero (a valid
/// `cidr` value / network address).
fn host_bits_clear(addr: &[u8], bits: u8) -> bool {
    let full = (bits / 8) as usize;
    let rem = (bits % 8) as u32;
    for (i, &byte) in addr.iter().enumerate() {
        if i < full {
            continue;
        }
        if i == full {
            if !high_bits_only(byte, rem) {
                return false;
            }
        } else if byte != 0 {
            return false;
        }
    }
    true
}

/// Zeroes every address bit at or beyond position `bits`, in place.
fn clear_host_bits(addr: &mut [u8; 16], bits: u8) {
    let full = (bits / 8) as usize;
    let rem = (bits % 8) as u32;
    for (i, byte) in addr.iter_mut().enumerate() {
        if i < full {
            continue;
        }
        if i == full && rem != 0 {
            *byte &= !(0xffu8 >> rem);
        } else {
            *byte = 0;
        }
    }
}

/// Parses a decimal `0..=255` octet with no leading `+`/`-`/whitespace.
fn parse_octet(s: &str) -> Option<u8> {
    if s.is_empty() || s.len() > 3 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse::<u16>().ok().filter(|&v| v <= 255).map(|v| v as u8)
}

/// Parses a dotted-decimal IPv4 address into four bytes. Requires exactly four
/// octets (no abbreviation — the caller handles `cidr`'s short forms).
fn parse_ipv4(s: &str) -> Option<[u8; 4]> {
    let mut out = [0u8; 4];
    let mut n = 0;
    for part in s.split('.') {
        if n == 4 {
            return None;
        }
        out[n] = parse_octet(part)?;
        n += 1;
    }
    (n == 4).then_some(out)
}

/// Parses one side of an IPv6 address (the groups before or after a `::`) into
/// `out`, allowing a trailing dotted-quad IPv4 (two groups). Returns the count
/// of 16-bit groups written.
fn parse_ipv6_side(part: &str, out: &mut [u16; 8]) -> Option<usize> {
    if part.is_empty() {
        return Some(0);
    }
    let mut idx = 0;
    let mut iter = part.split(':').peekable();
    while let Some(piece) = iter.next() {
        let is_last = iter.peek().is_none();
        if is_last && piece.contains('.') {
            let v4 = parse_ipv4(piece)?;
            if idx + 2 > 8 {
                return None;
            }
            out[idx] = (u16::from(v4[0]) << 8) | u16::from(v4[1]);
            out[idx + 1] = (u16::from(v4[2]) << 8) | u16::from(v4[3]);
            idx += 2;
        } else {
            if piece.is_empty()
                || piece.len() > 4
                || !piece.bytes().all(|b| b.is_ascii_hexdigit())
                || idx >= 8
            {
                return None;
            }
            out[idx] = u16::from_str_radix(piece, 16).ok()?;
            idx += 1;
        }
    }
    Some(idx)
}

/// Parses an IPv6 address (with optional embedded IPv4 tail and `::`
/// compression) into 16 bytes.
fn parse_ipv6(s: &str) -> Option<[u8; 16]> {
    let (head_str, tail_str, compressed) = match s.find("::") {
        Some(pos) => {
            // Only one "::" is allowed.
            if s[pos + 2..].contains("::") {
                return None;
            }
            (&s[..pos], &s[pos + 2..], true)
        }
        None => (s, "", false),
    };

    let mut head = [0u16; 8];
    let mut tail = [0u16; 8];
    let n_head = parse_ipv6_side(head_str, &mut head)?;
    let mut groups = [0u16; 8];
    if compressed {
        let n_tail = parse_ipv6_side(tail_str, &mut tail)?;
        if n_head + n_tail > 7 {
            // "::" must stand for at least one zero group.
            return None;
        }
        groups[..n_head].copy_from_slice(&head[..n_head]);
        for i in 0..n_tail {
            groups[8 - n_tail + i] = tail[i];
        }
    } else {
        if n_head != 8 {
            return None;
        }
        groups.copy_from_slice(&head);
    }

    let mut out = [0u8; 16];
    for i in 0..8 {
        out[i * 2] = (groups[i] >> 8) as u8;
        out[i * 2 + 1] = (groups[i] & 0xff) as u8;
    }
    Some(out)
}

/// Splits an `address[/bits]` string into the address and an optional mask.
fn split_mask(s: &str) -> (&str, Option<&str>) {
    match s.split_once('/') {
        Some((a, m)) => (a, Some(m)),
        None => (s, None),
    }
}

/// Parses an `inet` value: an IPv4 or IPv6 address with an optional `/bits`
/// mask (host bits are allowed and preserved).
pub fn parse_inet(s: &str) -> Option<NetAddr> {
    let s = s.trim();
    let (addr_str, mask_str) = split_mask(s);
    let (family, addr) = parse_address(addr_str)?;
    let max = if family == 4 { 32 } else { 128 };
    let bits = match mask_str {
        Some(m) => {
            if m.is_empty() || !m.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let v: u16 = m.parse().ok()?;
            if v > u16::from(max) {
                return None;
            }
            v as u8
        }
        None => max,
    };
    Some(NetAddr { family, bits, addr })
}

/// Parses a `cidr` value: like `inet`, but accepts abbreviated IPv4
/// (`10.1` → `10.1.0.0`), defaults the mask from the written octets, and
/// rejects any bit set to the right of the mask.
pub fn parse_cidr(s: &str) -> Option<NetAddr> {
    let s = s.trim();
    let (addr_str, mask_str) = split_mask(s);

    // IPv4 may be abbreviated (1–4 octets); a missing mask defaults to the
    // number of written octets × 8.
    let (family, addr, default_bits) = if addr_str.contains(':') {
        let a = parse_ipv6(addr_str)?;
        (6u8, a, 128u8)
    } else {
        let mut octets = [0u8; 4];
        let mut n = 0;
        for part in addr_str.split('.') {
            if n == 4 {
                return None;
            }
            octets[n] = parse_octet(part)?;
            n += 1;
        }
        if n == 0 {
            return None;
        }
        let mut a = [0u8; 16];
        a[..4].copy_from_slice(&octets);
        (4u8, a, (n as u8) * 8)
    };
    let max: u8 = if family == 4 { 32 } else { 128 };
    let bits = match mask_str {
        Some(m) => {
            if m.is_empty() || !m.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let v: u16 = m.parse().ok()?;
            if v > u16::from(max) {
                return None;
            }
            v as u8
        }
        None => default_bits,
    };
    if !host_bits_clear(&addr[..if family == 4 { 4 } else { 16 }], bits) {
        return None;
    }
    Some(NetAddr { family, bits, addr })
}

/// Parses a bare address (no mask) as either IPv4 (exactly four octets) or
/// IPv6, returning the family and 16-byte buffer.
fn parse_address(s: &str) -> Option<(u8, [u8; 16])> {
    if s.contains(':') {
        Some((6, parse_ipv6(s)?))
    } else {
        let v4 = parse_ipv4(s)?;
        let mut a = [0u8; 16];
        a[..4].copy_from_slice(&v4);
        Some((4, a))
    }
}

/// Renders an address into `out`, appending `/bits` when `show_mask` is set or
/// the mask is not the family default (matching `inet` vs `cidr` output).
pub fn format_addr(net: &NetAddr, always_mask: bool, out: &mut impl fmt::Write) -> fmt::Result {
    if net.family == 4 {
        write!(
            out,
            "{}.{}.{}.{}",
            net.addr[0], net.addr[1], net.addr[2], net.addr[3]
        )?;
    } else {
        format_ipv6(&net.addr, out)?;
    }
    if always_mask || net.bits != net.max_bits() {
        write!(out, "/{}", net.bits)?;
    }
    Ok(())
}

/// Renders 16 bytes as canonical IPv6 text (RFC 5952: lowercase, longest
/// zero-run compressed to `::`, leftmost on a tie, and a dotted-quad tail for
/// a v4-mapped `::ffff:a.b.c.d`).
fn format_ipv6(addr: &[u8; 16], out: &mut impl fmt::Write) -> fmt::Result {
    let groups: [u16; 8] =
        core::array::from_fn(|i| (u16::from(addr[i * 2]) << 8) | u16::from(addr[i * 2 + 1]));

    // v4-mapped: ::ffff:a.b.c.d
    if groups[..5].iter().all(|&g| g == 0) && groups[5] == 0xffff {
        return write!(
            out,
            "::ffff:{}.{}.{}.{}",
            addr[12], addr[13], addr[14], addr[15]
        );
    }

    // Find the longest run of zero groups (length ≥ 2), leftmost on a tie.
    let (mut best_start, mut best_len) = (usize::MAX, 0usize);
    let mut i = 0;
    while i < 8 {
        if groups[i] == 0 {
            let start = i;
            while i < 8 && groups[i] == 0 {
                i += 1;
            }
            let len = i - start;
            if len > best_len {
                best_len = len;
                best_start = start;
            }
        } else {
            i += 1;
        }
    }
    // Groups joined by ':', with a `::` standing in for the longest zero run.
    // Splitting on the run keeps the colon accounting trivial: the left and
    // right sides each join without a leading/trailing colon, and `::` bridges
    // them (so `["1"] :: [] → "1::"`, `[] :: ["1"] → "::1"`, `[] :: [] → "::"`).
    fn join(out: &mut impl fmt::Write, groups: &[u16]) -> fmt::Result {
        for (i, g) in groups.iter().enumerate() {
            if i > 0 {
                out.write_char(':')?;
            }
            write!(out, "{g:x}")?;
        }
        Ok(())
    }
    if best_len >= 2 {
        join(out, &groups[..best_start])?;
        out.write_str("::")?;
        join(out, &groups[best_start + best_len..])
    } else {
        join(out, &groups)
    }
}

// --- macaddr / macaddr8 ---------------------------------------------------

/// Reads `n` hex bytes out of a string that may use `:`, `-`, or `.`
/// separators, or none, in PostgreSQL's accepted groupings. Returns the bytes
/// or `None` on any malformed input.
fn parse_mac_bytes(s: &str, n: usize, out: &mut [u8]) -> Option<()> {
    // Collect hex nibbles, ignoring the allowed separators.
    let mut nibbles = [0u8; 16];
    let mut count = 0;
    for b in s.bytes() {
        match b {
            b':' | b'-' | b'.' => continue,
            _ => {
                let d = (b as char).to_digit(16)? as u8;
                if count == nibbles.len() {
                    return None;
                }
                nibbles[count] = d;
                count += 1;
            }
        }
    }
    if count != n * 2 {
        return None;
    }
    for i in 0..n {
        out[i] = nibbles[i * 2] << 4 | nibbles[i * 2 + 1];
    }
    Some(())
}

/// Parses a `macaddr` (six bytes) in any of PostgreSQL's accepted spellings.
pub fn parse_macaddr(s: &str) -> Option<[u8; 6]> {
    let mut out = [0u8; 6];
    parse_mac_bytes(s.trim(), 6, &mut out)?;
    Some(out)
}

/// Parses a `macaddr8` (eight bytes). A six-byte input is widened to EUI-64 by
/// inserting `ff:fe` in the middle, as PostgreSQL does.
pub fn parse_macaddr8(s: &str) -> Option<[u8; 8]> {
    let s = s.trim();
    // Count hex digits to decide 6- vs 8-byte form.
    let hexdigits = s.bytes().filter(|b| b.is_ascii_hexdigit()).count();
    if hexdigits == 12 {
        let mut six = [0u8; 6];
        parse_mac_bytes(s, 6, &mut six)?;
        Some([six[0], six[1], six[2], 0xff, 0xfe, six[3], six[4], six[5]])
    } else {
        let mut out = [0u8; 8];
        parse_mac_bytes(s, 8, &mut out)?;
        Some(out)
    }
}

/// Sets the top `bits` bits of `addr[..len]` to one, the rest to zero.
fn set_prefix_ones(addr: &mut [u8; 16], bits: u8, len: usize) {
    let full = (bits / 8) as usize;
    let rem = bits % 8;
    for (i, byte) in addr[..len].iter_mut().enumerate() {
        if i < full {
            *byte = 0xff;
        } else if i == full && rem != 0 {
            *byte = 0xffu8 << (8 - rem);
        }
    }
}

/// Renders a `cidr` in `abbrev()` form: an IPv4 network drops trailing
/// all-zero octets (`10.1.0.0/16` → `10.1/16`); IPv6 uses the canonical
/// compressed form with its mask.
pub fn format_cidr_abbrev(net: &NetAddr, out: &mut impl fmt::Write) -> fmt::Result {
    if net.family == 6 {
        return format_addr(net, true, out);
    }
    let octets = usize::from(net.bits.div_ceil(8)).max(1);
    for i in 0..octets {
        if i > 0 {
            out.write_char('.')?;
        }
        write!(out, "{}", net.addr[i])?;
    }
    write!(out, "/{}", net.bits)
}

/// The number of leading bits two addresses share (over the family width).
fn common_prefix_bits(a: &[u8; 16], b: &[u8; 16], len: usize) -> u8 {
    let mut bits = 0u8;
    for i in 0..len {
        if a[i] == b[i] {
            bits += 8;
        } else {
            bits += (a[i] ^ b[i]).leading_zeros() as u8;
            break;
        }
    }
    bits
}

/// The smallest network containing both addresses (`inet_merge`). `None` if the
/// families differ.
pub fn inet_merge(a: &NetAddr, b: &NetAddr) -> Option<NetAddr> {
    if a.family != b.family {
        return None;
    }
    let bits = common_prefix_bits(&a.addr, &b.addr, a.addr_len());
    let mut out = NetAddr {
        family: a.family,
        bits,
        addr: a.addr,
    };
    clear_host_bits(&mut out.addr, bits);
    Some(out)
}

/// Renders MAC bytes as lowercase colon-separated hex.
pub fn format_mac(bytes: &[u8], out: &mut impl fmt::Write) -> fmt::Result {
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 {
            out.write_char(':')?;
        }
        write!(out, "{b:02x}")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inet(s: &str) -> String {
        let n = parse_inet(s).unwrap_or_else(|| panic!("parse inet {s}"));
        let mut out = String::new();
        format_addr(&n, false, &mut out).unwrap();
        out
    }
    fn cidr(s: &str) -> String {
        let n = parse_cidr(s).unwrap_or_else(|| panic!("parse cidr {s}"));
        let mut out = String::new();
        format_addr(&n, true, &mut out).unwrap();
        out
    }
    fn mac(s: &str) -> String {
        let b = parse_macaddr(s).unwrap_or_else(|| panic!("parse mac {s}"));
        let mut out = String::new();
        format_mac(&b, &mut out).unwrap();
        out
    }
    fn mac8(s: &str) -> String {
        let b = parse_macaddr8(s).unwrap_or_else(|| panic!("parse mac8 {s}"));
        let mut out = String::new();
        format_mac(&b, &mut out).unwrap();
        out
    }

    #[test]
    fn inet_roundtrips_match_postgres() {
        assert_eq!(inet("10.0.0.1"), "10.0.0.1"); // default /32 omitted
        assert_eq!(inet("10.0.0.1/8"), "10.0.0.1/8"); // host bits kept
        assert_eq!(inet("192.168.1.5/24"), "192.168.1.5/24");
        assert_eq!(inet("2001:db8::1"), "2001:db8::1"); // default /128 omitted
        assert_eq!(
            inet("2001:0db8:0000:0000:0000:0000:0000:0001"),
            "2001:db8::1"
        );
        assert_eq!(inet("::1"), "::1");
        assert_eq!(inet("::"), "::");
        assert_eq!(inet("fe80::1/64"), "fe80::1/64");
        assert_eq!(inet("::ffff:1.2.3.4"), "::ffff:1.2.3.4");
        // Leftmost longest zero-run compresses.
        assert_eq!(inet("1:0:0:1:0:0:0:1"), "1:0:0:1::1");
        assert_eq!(inet("2001:db8:0:0:1:0:0:1"), "2001:db8::1:0:0:1");
        assert!(parse_inet("zzz").is_none());
        assert!(parse_inet("256.1.1.1").is_none());
        assert!(parse_inet("10.1").is_none()); // inet needs a full quad
    }

    #[test]
    fn cidr_roundtrips_match_postgres() {
        assert_eq!(cidr("10.0.0.0/8"), "10.0.0.0/8");
        assert_eq!(cidr("10.1"), "10.1.0.0/16"); // abbreviated, mask from octets
        assert_eq!(cidr("192.168.1.0/24"), "192.168.1.0/24");
        assert_eq!(cidr("2001:db8::/32"), "2001:db8::/32");
        // Host bits set to the right of the mask are rejected.
        assert!(parse_cidr("192.168.1.5/24").is_none());
    }

    #[test]
    fn macaddr_roundtrips_match_postgres() {
        for form in [
            "08:00:2b:01:02:03",
            "08-00-2b-01-02-03",
            "08002b:010203",
            "08002b010203",
            "0800.2b01.0203",
            "08:00:2B:01:02:03",
        ] {
            assert_eq!(mac(form), "08:00:2b:01:02:03", "form {form}");
        }
        assert!(parse_macaddr("gg:00:2b:01:02:03").is_none());
        assert!(parse_macaddr("08:00:2b:01:02").is_none()); // too short
    }

    #[test]
    fn macaddr8_widens_to_eui64() {
        assert_eq!(mac8("08:00:2b:01:02:03:04:05"), "08:00:2b:01:02:03:04:05");
        // A six-byte input inserts ff:fe in the middle.
        assert_eq!(mac8("08:00:2b:01:02:03"), "08:00:2b:ff:fe:01:02:03");
    }
}
