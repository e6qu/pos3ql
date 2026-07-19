//! Row codec. Layout, all little-endian:
//!
//! ```text
//! u16 column-count | null bitmap (ceil(n/8) bytes) | non-null values
//! ```
//!
//! Fixed-width values by column type (bool 1, int4 4, int8/float8 8);
//! text is `u32 len` + UTF-8 bytes. The same encoding will be written
//! into SSTs, so it is versioned by the column count against the schema.

use crate::sql::eval::{sqlstate, SqlError};
use crate::sql::types::{ColType, Datum};
use crate::sql_err;

pub const MAX_COLUMNS: usize = 64;

pub fn encoded_len(values: &[Datum]) -> usize {
    let mut n = 2 + values.len().div_ceil(8);
    for v in values {
        n += match v {
            Datum::Null => 0,
            Datum::Bool(_) => 1,
            Datum::Int4(_) | Datum::Date(_) => 4,
            Datum::Int8(_) | Datum::Float8(_) | Datum::Timestamp(_) | Datum::Timestamptz(_) | Datum::Time(_) => 8,
            Datum::Interval(_) => 16,
            Datum::Uuid(_) => 16,
            Datum::Text(s) => 4 + s.len(),
            Datum::Json { text, .. } | Datum::Range { text, .. } => 4 + text.len(),
            Datum::Array { raw, .. } => 5 + raw.len(),
            Datum::Bytea(b) => 4 + b.len(),
            // sign(1) weight(2) dscale(2) ndigits(2) + packed digit bytes
            Datum::Numeric(nm) => 7 + nm.digits.len(),
        };
    }
    n
}

/// Encodes into `out`, which must be exactly `encoded_len` bytes.
pub fn encode(values: &[Datum], out: &mut [u8]) {
    debug_assert_eq!(out.len(), encoded_len(values));
    let n = values.len();
    out[..2].copy_from_slice(&(n as u16).to_le_bytes());
    let bitmap_len = n.div_ceil(8);
    let (bitmap, mut rest) = out[2..].split_at_mut(bitmap_len);
    bitmap.fill(0);
    for (i, v) in values.iter().enumerate() {
        if v.is_null() {
            bitmap[i / 8] |= 1 << (i % 8);
            continue;
        }
        let take;
        match v {
            Datum::Bool(b) => {
                rest[0] = u8::from(*b);
                take = 1;
            }
            Datum::Int4(x) => {
                rest[..4].copy_from_slice(&x.to_le_bytes());
                take = 4;
            }
            Datum::Int8(x) => {
                rest[..8].copy_from_slice(&x.to_le_bytes());
                take = 8;
            }
            Datum::Float8(x) => {
                rest[..8].copy_from_slice(&x.to_le_bytes());
                take = 8;
            }
            Datum::Text(s) => {
                rest[..4].copy_from_slice(&(s.len() as u32).to_le_bytes());
                rest[4..4 + s.len()].copy_from_slice(s.as_bytes());
                take = 4 + s.len();
            }
            Datum::Json { text, .. } | Datum::Range { text, .. } => {
                rest[..4].copy_from_slice(&(text.len() as u32).to_le_bytes());
                rest[4..4 + text.len()].copy_from_slice(text.as_bytes());
                take = 4 + text.len();
            }
            Datum::Array { elem, raw } => {
                let payload = 1 + raw.len();
                rest[..4].copy_from_slice(&(payload as u32).to_le_bytes());
                rest[4] = elem.code();
                rest[5..5 + raw.len()].copy_from_slice(raw);
                take = 4 + payload;
            }
            Datum::Date(x) => {
                rest[..4].copy_from_slice(&x.to_le_bytes());
                take = 4;
            }
            Datum::Interval(iv) => {
                rest[..4].copy_from_slice(&iv.months.to_le_bytes());
                rest[4..8].copy_from_slice(&iv.days.to_le_bytes());
                rest[8..16].copy_from_slice(&iv.micros.to_le_bytes());
                take = 16;
            }
            Datum::Timestamp(x) | Datum::Timestamptz(x) | Datum::Time(x) => {
                rest[..8].copy_from_slice(&x.to_le_bytes());
                take = 8;
            }
            Datum::Uuid(b) => {
                rest[..16].copy_from_slice(b);
                take = 16;
            }
            Datum::Bytea(b) => {
                rest[..4].copy_from_slice(&(b.len() as u32).to_le_bytes());
                rest[4..4 + b.len()].copy_from_slice(b);
                take = 4 + b.len();
            }
            Datum::Numeric(nm) => {
                rest[0] = match nm.sign {
                    crate::sql::numeric::Sign::Pos => 0,
                    crate::sql::numeric::Sign::Neg => 1,
                    crate::sql::numeric::Sign::NaN => 2,
                };
                rest[1..3].copy_from_slice(&nm.weight.to_le_bytes());
                rest[3..5].copy_from_slice(&nm.dscale.to_le_bytes());
                rest[5..7].copy_from_slice(&(nm.ndigits() as u16).to_le_bytes());
                rest[7..7 + nm.digits.len()].copy_from_slice(nm.digits);
                take = 7 + nm.digits.len();
            }
            Datum::Null => unreachable!(),
        }
        rest = &mut rest[take..];
    }
}

/// Decodes a row into `out` (at least as many slots as the schema has
/// columns). Text values borrow from `bytes`.
pub fn decode<'a>(
    bytes: &'a [u8],
    schema: &[ColType],
    out: &mut [Datum<'a>],
) -> Result<(), SqlError> {
    let corrupt = || sql_err!(sqlstate::PROTOCOL_VIOLATION, "corrupt row encoding");
    if bytes.len() < 2 {
        return Err(corrupt());
    }
    let n = u16::from_le_bytes([bytes[0], bytes[1]]) as usize;
    if n != schema.len() || out.len() < n {
        return Err(corrupt());
    }
    let bitmap_len = n.div_ceil(8);
    if bytes.len() < 2 + bitmap_len {
        return Err(corrupt());
    }
    let bitmap = &bytes[2..2 + bitmap_len];
    let mut at = 2 + bitmap_len;
    for i in 0..n {
        if bitmap[i / 8] & (1 << (i % 8)) != 0 {
            out[i] = Datum::Null;
            continue;
        }
        // int2/float4/varchar/bpchar share the byte layout of their storage
        // type (int4/float8/text), so they decode through the same arm.
        match schema[i] {
            ColType::Bool => {
                let b = bytes.get(at..at + 1).ok_or_else(corrupt)?;
                out[i] = Datum::Bool(b[0] != 0);
                at += 1;
            }
            ColType::Int4 | ColType::Int2 => {
                let b = bytes.get(at..at + 4).ok_or_else(corrupt)?;
                out[i] = Datum::Int4(i32::from_le_bytes(b.try_into().unwrap()));
                at += 4;
            }
            ColType::Int8 => {
                let b = bytes.get(at..at + 8).ok_or_else(corrupt)?;
                out[i] = Datum::Int8(i64::from_le_bytes(b.try_into().unwrap()));
                at += 8;
            }
            ColType::Float8 | ColType::Float4 => {
                let b = bytes.get(at..at + 8).ok_or_else(corrupt)?;
                out[i] = Datum::Float8(f64::from_le_bytes(b.try_into().unwrap()));
                at += 8;
            }
            ColType::Text | ColType::Varchar | ColType::Bpchar => {
                let b = bytes.get(at..at + 4).ok_or_else(corrupt)?;
                let len = u32::from_le_bytes(b.try_into().unwrap()) as usize;
                at += 4;
                let raw = bytes.get(at..at + len).ok_or_else(corrupt)?;
                at += len;
                let s = core::str::from_utf8(raw).map_err(|_| corrupt())?;
                out[i] = Datum::Text(s);
            }
            ColType::Date => {
                let b = bytes.get(at..at + 4).ok_or_else(corrupt)?;
                out[i] = Datum::Date(i32::from_le_bytes(b.try_into().unwrap()));
                at += 4;
            }
            ColType::Timestamp => {
                let b = bytes.get(at..at + 8).ok_or_else(corrupt)?;
                out[i] = Datum::Timestamp(i64::from_le_bytes(b.try_into().unwrap()));
                at += 8;
            }
            ColType::Timestamptz => {
                let b = bytes.get(at..at + 8).ok_or_else(corrupt)?;
                out[i] = Datum::Timestamptz(i64::from_le_bytes(b.try_into().unwrap()));
                at += 8;
            }
            ColType::Time => {
                let b = bytes.get(at..at + 8).ok_or_else(corrupt)?;
                out[i] = Datum::Time(i64::from_le_bytes(b.try_into().unwrap()));
                at += 8;
            }
            ColType::Array(elem) => {
                let b = bytes.get(at..at + 4).ok_or_else(corrupt)?;
                let payload = u32::from_le_bytes(b.try_into().unwrap()) as usize;
                at += 4;
                // Skip the element-type code byte; the schema is authoritative.
                let raw = bytes.get(at + 1..at + payload).ok_or_else(corrupt)?;
                at += payload;
                out[i] = Datum::Array { elem, raw };
            }
            ColType::Json | ColType::Jsonb => {
                let b = bytes.get(at..at + 4).ok_or_else(corrupt)?;
                let len = u32::from_le_bytes(b.try_into().unwrap()) as usize;
                at += 4;
                let raw = bytes.get(at..at + len).ok_or_else(corrupt)?;
                at += len;
                let s = core::str::from_utf8(raw).map_err(|_| corrupt())?;
                out[i] = Datum::Json { text: s, jsonb: matches!(schema[i], ColType::Jsonb) };
            }
            ColType::Range(kind) => {
                let b = bytes.get(at..at + 4).ok_or_else(corrupt)?;
                let len = u32::from_le_bytes(b.try_into().unwrap()) as usize;
                at += 4;
                let raw = bytes.get(at..at + len).ok_or_else(corrupt)?;
                at += len;
                let s = core::str::from_utf8(raw).map_err(|_| corrupt())?;
                out[i] = Datum::Range { text: s, kind };
            }
            ColType::Interval => {
                let mo = bytes.get(at..at + 4).ok_or_else(corrupt)?;
                let dy = bytes.get(at + 4..at + 8).ok_or_else(corrupt)?;
                let us = bytes.get(at + 8..at + 16).ok_or_else(corrupt)?;
                out[i] = Datum::Interval(crate::sql::types::Interval {
                    months: i32::from_le_bytes(mo.try_into().unwrap()),
                    days: i32::from_le_bytes(dy.try_into().unwrap()),
                    micros: i64::from_le_bytes(us.try_into().unwrap()),
                });
                at += 16;
            }
            ColType::Uuid => {
                let b = bytes.get(at..at + 16).ok_or_else(corrupt)?;
                out[i] = Datum::Uuid(b.try_into().unwrap());
                at += 16;
            }
            ColType::Bytea => {
                let b = bytes.get(at..at + 4).ok_or_else(corrupt)?;
                let len = u32::from_le_bytes(b.try_into().unwrap()) as usize;
                at += 4;
                let raw = bytes.get(at..at + len).ok_or_else(corrupt)?;
                at += len;
                out[i] = Datum::Bytea(raw);
            }
            ColType::Numeric => {
                let h = bytes.get(at..at + 7).ok_or_else(corrupt)?;
                let sign = match h[0] {
                    0 => crate::sql::numeric::Sign::Pos,
                    1 => crate::sql::numeric::Sign::Neg,
                    2 => crate::sql::numeric::Sign::NaN,
                    _ => return Err(corrupt()),
                };
                let weight = i16::from_le_bytes([h[1], h[2]]);
                let dscale = u16::from_le_bytes([h[3], h[4]]);
                let ndigits = u16::from_le_bytes([h[5], h[6]]) as usize;
                at += 7;
                let raw = bytes.get(at..at + ndigits * 2).ok_or_else(corrupt)?;
                at += ndigits * 2;
                out[i] = Datum::Numeric(crate::sql::numeric::Numeric {
                    sign, weight, dscale, digits: raw,
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_all_types_and_nulls() {
        let schema = [
            ColType::Bool,
            ColType::Int4,
            ColType::Int8,
            ColType::Float8,
            ColType::Text,
            ColType::Text,
        ];
        let values = [
            Datum::Bool(true),
            Datum::Int4(-7),
            Datum::Null,
            Datum::Float8(2.5),
            Datum::Text("hello, 世界"),
            Datum::Null,
        ];
        let mut buf = vec![0u8; encoded_len(&values)];
        encode(&values, &mut buf);
        let mut out = [Datum::Null; MAX_COLUMNS];
        decode(&buf, &schema, &mut out).unwrap();
        assert_eq!(&out[..6], &values);
    }

    #[test]
    fn truncated_bytes_are_an_error_not_a_panic() {
        let schema = [ColType::Int8];
        let values = [Datum::Int8(1)];
        let mut buf = vec![0u8; encoded_len(&values)];
        encode(&values, &mut buf);
        for cut in 0..buf.len() {
            let mut out = [Datum::Null; 1];
            assert!(decode(&buf[..cut], &schema, &mut out).is_err(), "cut={cut}");
        }
    }

    #[test]
    fn schema_mismatch_is_an_error() {
        let values = [Datum::Int4(1)];
        let mut buf = vec![0u8; encoded_len(&values)];
        encode(&values, &mut buf);
        let mut out = [Datum::Null; 2];
        assert!(decode(&buf, &[ColType::Int4, ColType::Int4], &mut out).is_err());
    }
}
