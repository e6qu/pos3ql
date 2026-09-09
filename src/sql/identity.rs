//! PostgreSQL tuple and command identity scalar semantics.

use crate::sql::eval::{SqlError, sqlstate};
use crate::sql::types::Tid;
use crate::sql_err;

fn invalid(value: &str, type_name: &str) -> SqlError {
    sql_err!(
        sqlstate::INVALID_TEXT_REPRESENTATION,
        "invalid input syntax for type {}: \"{}\"",
        type_name,
        value
    )
}

fn out_of_range(value: &str, type_name: &str) -> SqlError {
    sql_err!(
        sqlstate::NUMERIC_OUT_OF_RANGE,
        "value \"{}\" is out of range for type {}",
        value,
        type_name
    )
}

pub fn parse_tid(value: &str) -> Result<Tid, SqlError> {
    // PostgreSQL's historical parser scans to the first opening delimiter and
    // ignores bytes after the closing one. Preserve that client-visible quirk.
    let Some(close) = value.find(')') else {
        return Err(invalid(value, "tid"));
    };
    let Some(open) = value[..close].find('(') else {
        return Err(invalid(value, "tid"));
    };
    let body = &value[open + 1..close];
    let Some(comma) = body.find(',') else {
        return Err(invalid(value, "tid"));
    };
    let block = &body[..comma];
    let offset = &body[comma + 1..];
    let block = if block.trim_start().starts_with('-') {
        let signed = block
            .trim_start()
            .parse::<i32>()
            .map_err(|_| invalid(value, "tid"))?;
        signed as u32
    } else {
        block
            .trim_start()
            .parse::<u32>()
            .map_err(|_| invalid(value, "tid"))?
    };
    let offset = offset
        .trim_start()
        .parse::<u16>()
        .map_err(|_| invalid(value, "tid"))?;
    Ok(Tid { block, offset })
}

pub fn parse_cid(value: &str) -> Result<u32, SqlError> {
    let trimmed = value.trim();
    let (negative, digits) = match trimmed.as_bytes().first() {
        Some(b'-') => (true, &trimmed[1..]),
        Some(b'+') => (false, &trimmed[1..]),
        Some(_) => (false, trimmed),
        None => return Err(invalid(value, "cid")),
    };
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid(value, "cid"));
    }
    let magnitude = digits
        .parse::<u64>()
        .map_err(|_| out_of_range(value, "cid"))?;
    if negative {
        if magnitude > 1u64 << 31 {
            return Err(out_of_range(value, "cid"));
        }
        Ok((-(magnitude as i64) as i32) as u32)
    } else if magnitude <= u64::from(u32::MAX) {
        Ok(magnitude as u32)
    } else {
        Err(out_of_range(value, "cid"))
    }
}

fn mix(mut a: u32, mut b: u32, mut c: u32) -> (u32, u32, u32) {
    a = a.wrapping_sub(c);
    a ^= c.rotate_left(4);
    c = c.wrapping_add(b);
    b = b.wrapping_sub(a);
    b ^= a.rotate_left(6);
    a = a.wrapping_add(c);
    c = c.wrapping_sub(b);
    c ^= b.rotate_left(8);
    b = b.wrapping_add(a);
    a = a.wrapping_sub(c);
    a ^= c.rotate_left(16);
    c = c.wrapping_add(b);
    b = b.wrapping_sub(a);
    b ^= a.rotate_left(19);
    a = a.wrapping_add(c);
    c = c.wrapping_sub(b);
    c ^= b.rotate_left(4);
    b = b.wrapping_add(a);
    (a, b, c)
}

fn final_mix(mut a: u32, mut b: u32, mut c: u32) -> (u32, u32) {
    c ^= b;
    c = c.wrapping_sub(b.rotate_left(14));
    a ^= c;
    a = a.wrapping_sub(c.rotate_left(11));
    b ^= a;
    b = b.wrapping_sub(a.rotate_left(25));
    c ^= b;
    c = c.wrapping_sub(b.rotate_left(16));
    a ^= c;
    a = a.wrapping_sub(c.rotate_left(4));
    b ^= a;
    b = b.wrapping_sub(a.rotate_left(14));
    c ^= b;
    c = c.wrapping_sub(b.rotate_left(24));
    (b, c)
}

fn hash_bytes(mut bytes: &[u8], seed: u64) -> (u32, u32) {
    let initial = 0x9e37_79b9u32
        .wrapping_add(bytes.len() as u32)
        .wrapping_add(3_923_095);
    let (mut a, mut b, mut c) = (initial, initial, initial);
    if seed != 0 {
        a = a.wrapping_add((seed >> 32) as u32);
        b = b.wrapping_add(seed as u32);
        (a, b, c) = mix(a, b, c);
    }
    while bytes.len() >= 12 {
        a = a.wrapping_add(u32::from_ne_bytes(bytes[..4].try_into().unwrap()));
        b = b.wrapping_add(u32::from_ne_bytes(bytes[4..8].try_into().unwrap()));
        c = c.wrapping_add(u32::from_ne_bytes(bytes[8..12].try_into().unwrap()));
        (a, b, c) = mix(a, b, c);
        bytes = &bytes[12..];
    }
    for (index, byte) in bytes.iter().copied().enumerate() {
        #[cfg(target_endian = "little")]
        let shift = (index % 4) * 8;
        #[cfg(target_endian = "big")]
        let shift = (3 - index % 4) * 8;
        match index / 4 {
            0 => a = a.wrapping_add(u32::from(byte) << shift),
            1 => b = b.wrapping_add(u32::from(byte) << shift),
            _ => unreachable!(),
        }
    }
    final_mix(a, b, c)
}

fn hash_u32(value: u32, seed: u64) -> (u32, u32) {
    let initial = 0x9e37_79b9u32.wrapping_add(4).wrapping_add(3_923_095);
    let (mut a, mut b, mut c) = (initial, initial, initial);
    if seed != 0 {
        a = a.wrapping_add((seed >> 32) as u32);
        b = b.wrapping_add(seed as u32);
        (a, b, c) = mix(a, b, c);
    }
    final_mix(a.wrapping_add(value), b, c)
}

pub(crate) fn hash_uint32(value: u32) -> u32 {
    hash_u32(value, 0).1
}

pub(crate) fn hash_uint32_extended(value: u32, seed: i64) -> u64 {
    let (high, low) = hash_u32(value, seed as u64);
    (u64::from(high) << 32) | u64::from(low)
}

pub(crate) fn hash_int64_input(value: i64) -> u32 {
    let high = (value >> 32) as u32;
    (value as u32) ^ if value >= 0 { high } else { !high }
}

fn tid_native_bytes(value: Tid) -> [u8; 6] {
    let high = (value.block >> 16) as u16;
    let low = value.block as u16;
    let mut bytes = [0; 6];
    bytes[..2].copy_from_slice(&high.to_ne_bytes());
    bytes[2..4].copy_from_slice(&low.to_ne_bytes());
    bytes[4..].copy_from_slice(&value.offset.to_ne_bytes());
    bytes
}

pub fn hash_tid(value: Tid) -> i32 {
    hash_bytes(&tid_native_bytes(value), 0).1 as i32
}

pub fn hash_tid_extended(value: Tid, seed: i64) -> i64 {
    let (high, low) = hash_bytes(&tid_native_bytes(value), seed as u64);
    ((u64::from(high) << 32) | u64::from(low)) as i64
}

pub fn hash_cid(value: u32) -> i32 {
    hash_u32(value, 0).1 as i32
}

pub fn hash_cid_extended(value: u32, seed: i64) -> i64 {
    let (high, low) = hash_u32(value, seed as u64);
    ((u64::from(high) << 32) | u64::from(low)) as i64
}

pub fn hash_bytea(value: &[u8]) -> i32 {
    hash_bytes(value, 0).1 as i32
}

pub fn hash_bytea_extended(value: &[u8], seed: i64) -> i64 {
    let (high, low) = hash_bytes(value, seed as u64);
    ((u64::from(high) << 32) | u64::from(low)) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tid_boundaries_and_legacy_negative_block() {
        assert_eq!(
            parse_tid("(0,0)").unwrap(),
            Tid {
                block: 0,
                offset: 0
            }
        );
        assert_eq!(
            parse_tid("(-1,65535)").unwrap(),
            Tid {
                block: u32::MAX,
                offset: u16::MAX,
            }
        );
        assert!(parse_tid("(4294967296,1)").is_err());
        assert!(parse_tid("(1,65536)").is_err());
        assert!(parse_tid("(0,-1)").is_err());
        assert_eq!(
            parse_tid("prefix( 1, 2)trailing").unwrap(),
            Tid {
                block: 1,
                offset: 2,
            }
        );
        assert!(parse_tid("(1 ,2)").is_err());
        assert!(parse_tid("(1,2 )").is_err());
        assert!(parse_tid(")(1,2)").is_err());
    }

    #[test]
    fn cid_boundaries_distinguish_syntax_from_range() {
        assert_eq!(parse_cid("  +42 ").unwrap(), 42);
        assert_eq!(parse_cid("-1").unwrap(), u32::MAX);
        assert_eq!(parse_cid("-2147483648").unwrap(), 1 << 31);
        assert_eq!(parse_cid("4294967296").unwrap_err().sqlstate, "22003");
        assert_eq!(parse_cid("42x").unwrap_err().sqlstate, "22P02");
    }

    #[test]
    fn hashes_match_postgresql_18() {
        let tid = Tid {
            block: 4,
            offset: 2,
        };
        assert_eq!(
            hash_tid(Tid {
                block: 0,
                offset: 0
            }),
            874_415_716
        );
        assert_eq!(hash_tid(tid), -675_576_837);
        assert_eq!(hash_tid_extended(tid, 123), 6_215_537_459_233_285_304);
        assert_eq!(hash_cid(42), 1_509_752_520);
        assert_eq!(hash_cid_extended(42, 0), 8_010_225_493_015_854_792);
        assert_eq!(hash_cid_extended(42, 123), -8_586_172_854_235_281_246);
        assert_eq!(hash_bytea(&[]), -1_477_818_771);
        assert_eq!(hash_bytea(&[0]), -1_062_527_899);
        assert_eq!(
            hash_bytea(&[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11]),
            1_048_759_404
        );
        let longer = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13];
        assert_eq!(hash_bytea(&longer), 1_547_648_725);
        assert_eq!(hash_bytea_extended(&longer, 123), 2_489_280_409_088_615_387);
    }
}
