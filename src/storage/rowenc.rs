//! Row codec. Layout, all little-endian:
//!
//! ```text
//! u16 column-count | null bitmap (ceil(n/8) bytes) | non-null values
//! ```
//!
//! Fixed-width values by column type (bool 1, int4 4, int8/float8 8);
//! text is `u32 len` + UTF-8 bytes. The same encoding will be written
//! into SSTs, so it is versioned by the column count against the schema.

use crate::sql::eval::{SqlError, sqlstate};
use crate::sql::types::{ColType, Datum};
use crate::sql_err;

pub(crate) const MAX_COLUMNS: usize = 64;

pub(crate) fn encoded_len(values: &[Datum]) -> usize {
    let mut n = 2 + values.len().div_ceil(8);
    for v in values {
        n += match v {
            // Records are transient values, never stored in a row (a column
            // cannot be composite-typed here).
            Datum::Record(_) => unreachable!("record cannot be a stored column value"),
            Datum::Int2Vector(_) => unreachable!("int2vector cannot be a stored column value"),
            Datum::Null => 0,
            Datum::Bool(_) => 1,
            Datum::Int2(_) | Datum::Int4(_) | Datum::Date(_) => 4,
            // float4 keeps the historical 8-byte float8 layout (see the decode
            // side); the schema narrows it back to f32.
            Datum::Int8(_)
            | Datum::Float4(_)
            | Datum::Float8(_)
            | Datum::Timestamp(_)
            | Datum::Timestamptz(_)
            | Datum::Time(_) => 8,
            Datum::Timetz(..) => 12,
            Datum::Interval(_) => 16,
            Datum::Uuid(_) => 16,
            // family(1) + mask bits(1) + 16 address bytes.
            Datum::Inet(_) | Datum::Cidr(_) => 18,
            Datum::Macaddr(_) => 6,
            Datum::Macaddr8(_) => 8,
            Datum::Text(s) | Datum::Bpchar(s) => 4 + s.len(),
            Datum::Json { text, .. }
            | Datum::Range { text, .. }
            | Datum::Multirange { text, .. } => 4 + text.len(),
            // 4-byte payload length, 1 flag byte (varying), then the bit chars.
            Datum::Bit { bits, .. } => 5 + bits.len(),
            Datum::Array { raw, .. } => 5 + raw.len(),
            Datum::Bytea(b) => 4 + b.len(),
            // sign(1) weight(2) dscale(2) ndigits(2) + packed digit bytes
            Datum::Numeric(nm) => 7 + nm.digits.len(),
            // sort key (8-byte f64) + 4-byte label length + label bytes. The
            // slot is not stored: it comes from the column's schema entry.
            Datum::Enum { label, .. } => 12 + label.len(),
        };
    }
    n
}

/// Encodes into `out`, which must be exactly `encoded_len` bytes.
pub(crate) fn encode(values: &[Datum], out: &mut [u8]) {
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
            Datum::Record(_) => unreachable!("record cannot be a stored column value"),
            Datum::Int2Vector(_) => unreachable!("int2vector cannot be a stored column value"),
            Datum::Bool(b) => {
                rest[0] = u8::from(*b);
                take = 1;
            }
            Datum::Int4(x) => {
                rest[..4].copy_from_slice(&x.to_le_bytes());
                take = 4;
            }
            Datum::Int2(x) => {
                rest[..4].copy_from_slice(&(*x as i32).to_le_bytes());
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
            // Widened to f64 for the historical 8-byte layout; the f32 value is
            // exactly representable, so decode narrows it back losslessly.
            Datum::Float4(x) => {
                rest[..8].copy_from_slice(&(*x as f64).to_le_bytes());
                take = 8;
            }
            Datum::Text(s) | Datum::Bpchar(s) => {
                rest[..4].copy_from_slice(&(s.len() as u32).to_le_bytes());
                rest[4..4 + s.len()].copy_from_slice(s.as_bytes());
                take = 4 + s.len();
            }
            Datum::Json { text, .. }
            | Datum::Range { text, .. }
            | Datum::Multirange { text, .. } => {
                rest[..4].copy_from_slice(&(text.len() as u32).to_le_bytes());
                rest[4..4 + text.len()].copy_from_slice(text.as_bytes());
                take = 4 + text.len();
            }
            Datum::Array { element, raw } => {
                let payload = 1 + raw.len();
                rest[..4].copy_from_slice(&(payload as u32).to_le_bytes());
                rest[4] = element.code();
                rest[5..5 + raw.len()].copy_from_slice(raw);
                take = 4 + payload;
            }
            Datum::Date(x) => {
                rest[..4].copy_from_slice(&x.to_le_bytes());
                take = 4;
            }
            Datum::Interval(interval) => {
                rest[..4].copy_from_slice(&interval.months.to_le_bytes());
                rest[4..8].copy_from_slice(&interval.days.to_le_bytes());
                rest[8..16].copy_from_slice(&interval.micros.to_le_bytes());
                take = 16;
            }
            Datum::Timetz(t, zone) => {
                rest[..8].copy_from_slice(&t.to_le_bytes());
                rest[8..12].copy_from_slice(&zone.to_le_bytes());
                take = 12;
            }
            Datum::Timestamp(x) | Datum::Timestamptz(x) | Datum::Time(x) => {
                rest[..8].copy_from_slice(&x.to_le_bytes());
                take = 8;
            }
            Datum::Uuid(b) => {
                rest[..16].copy_from_slice(b);
                take = 16;
            }
            Datum::Inet(net) | Datum::Cidr(net) => {
                rest[0] = net.family();
                rest[1] = net.bits();
                rest[2..18].copy_from_slice(net.addr());
                take = 18;
            }
            Datum::Macaddr(b) => {
                rest[..6].copy_from_slice(b);
                take = 6;
            }
            Datum::Macaddr8(b) => {
                rest[..8].copy_from_slice(b);
                take = 8;
            }
            Datum::Bytea(b) => {
                rest[..4].copy_from_slice(&(b.len() as u32).to_le_bytes());
                rest[4..4 + b.len()].copy_from_slice(b);
                take = 4 + b.len();
            }
            Datum::Bit { bits, varying } => {
                let payload = 1 + bits.len();
                rest[..4].copy_from_slice(&(payload as u32).to_le_bytes());
                rest[4] = u8::from(*varying);
                rest[5..5 + bits.len()].copy_from_slice(bits.as_bytes());
                take = 4 + payload;
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
            Datum::Enum { sort, label, .. } => {
                rest[..8].copy_from_slice(&sort.to_le_bytes());
                rest[8..12].copy_from_slice(&(label.len() as u32).to_le_bytes());
                rest[12..12 + label.len()].copy_from_slice(label.as_bytes());
                take = 12 + label.len();
            }
            Datum::Null => unreachable!(),
        }
        rest = &mut rest[take..];
    }
}

/// Splits a canonical stored row into its physical column payloads.  The
/// payloads exclude the row header and null bitmap, so a columnar SST can
/// retain exactly the bytes decoded by [`decode`] without constructing Datum
/// values.  `payloads` and `nulls` are caller-owned fixed scratch.
pub(crate) fn encoded_columns<'a>(
    bytes: &'a [u8],
    schema: &[ColType],
    payloads: &mut [&'a [u8]; MAX_COLUMNS],
    nulls: &mut [bool; MAX_COLUMNS],
) -> Result<(), SqlError> {
    let corrupt = || sql_err!(sqlstate::PROTOCOL_VIOLATION, "corrupt row encoding");
    if bytes.len() < 2 || schema.len() > MAX_COLUMNS {
        return Err(corrupt());
    }
    let count = u16::from_le_bytes([bytes[0], bytes[1]]) as usize;
    if count != schema.len() {
        return Err(corrupt());
    }
    let bitmap_len = count.div_ceil(8);
    if bytes.len() < 2 + bitmap_len {
        return Err(corrupt());
    }
    let bitmap = &bytes[2..2 + bitmap_len];
    let mut at = 2 + bitmap_len;
    for column in 0..count {
        let is_null = bitmap[column / 8] & (1 << (column % 8)) != 0;
        nulls[column] = is_null;
        if is_null {
            payloads[column] = &[];
            continue;
        }
        let length = encoded_value_len(&bytes[at..], schema[column])?;
        let end = at.checked_add(length).ok_or_else(corrupt)?;
        payloads[column] = bytes.get(at..end).ok_or_else(corrupt)?;
        at = end;
    }
    if at != bytes.len() {
        return Err(corrupt());
    }
    Ok(())
}

/// Length of one non-null physical column payload at the beginning of `bytes`.
/// This is shared by row decoding and the PAX reassembler so both formats
/// reject the same malformed variable-length values.
pub(crate) fn encoded_value_len(bytes: &[u8], column: ColType) -> Result<usize, SqlError> {
    let corrupt = || sql_err!(sqlstate::PROTOCOL_VIOLATION, "corrupt row encoding");
    let fixed = match column {
        ColType::Bool => Some(1),
        ColType::Int2 | ColType::Int4 | ColType::Oid | ColType::Date => Some(4),
        ColType::Int8
        | ColType::Float4
        | ColType::Float8
        | ColType::Timestamp
        | ColType::Timestamptz
        | ColType::Time => Some(8),
        ColType::Timetz => Some(12),
        ColType::Interval | ColType::Uuid => Some(16),
        ColType::Inet | ColType::Cidr => Some(18),
        ColType::Macaddr => Some(6),
        ColType::Macaddr8 => Some(8),
        ColType::Numeric => {
            let header = bytes.get(..7).ok_or_else(corrupt)?;
            Some(7 + u16::from_le_bytes([header[5], header[6]]) as usize * 2)
        }
        ColType::Text
        | ColType::Name
        | ColType::Varchar
        | ColType::Bpchar
        | ColType::Json
        | ColType::Jsonb
        | ColType::Range(_)
        | ColType::Multirange(_)
        | ColType::Bytea => {
            let length = bytes.get(..4).ok_or_else(corrupt)?;
            Some(4 + u32::from_le_bytes(length.try_into().unwrap()) as usize)
        }
        ColType::Array(_) | ColType::Bit { .. } => {
            let length = bytes.get(..4).ok_or_else(corrupt)?;
            let payload = u32::from_le_bytes(length.try_into().unwrap()) as usize;
            if payload == 0 {
                return Err(corrupt());
            }
            Some(4 + payload)
        }
        ColType::Enum(_) => {
            let length = bytes.get(8..12).ok_or_else(corrupt)?;
            Some(12 + u32::from_le_bytes(length.try_into().unwrap()) as usize)
        }
        ColType::Void | ColType::Int2Vector | ColType::Record => return Err(corrupt()),
    };
    let length = fixed.expect("all stored types have a length");
    if bytes.len() < length {
        return Err(corrupt());
    }
    Ok(length)
}

/// Decodes a row into `out` (at least as many slots as the schema has
/// columns). Text values borrow from `bytes`.
pub(crate) fn decode<'a>(
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
            ColType::Void => return Err(corrupt()),
            ColType::Int2Vector => {
                return Err(sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "int2vector cannot be decoded as a stored column"
                ));
            }
            ColType::Bool => {
                let b = bytes.get(at..at + 1).ok_or_else(corrupt)?;
                out[i] = Datum::Bool(b[0] != 0);
                at += 1;
            }
            ColType::Int4 | ColType::Int2 | ColType::Oid => {
                let b = bytes.get(at..at + 4).ok_or_else(corrupt)?;
                let x = i32::from_le_bytes(b.try_into().unwrap());
                // The 4-byte layout is historical; the schema narrows. A
                // stored int2 is range-checked at write, so the cast holds.
                out[i] = if matches!(schema[i], ColType::Int2) {
                    Datum::Int2(x as i16)
                } else {
                    Datum::Int4(x)
                };
                at += 4;
            }
            ColType::Int8 => {
                let b = bytes.get(at..at + 8).ok_or_else(corrupt)?;
                out[i] = Datum::Int8(i64::from_le_bytes(b.try_into().unwrap()));
                at += 8;
            }
            ColType::Float8 | ColType::Float4 => {
                let b = bytes.get(at..at + 8).ok_or_else(corrupt)?;
                let x = f64::from_le_bytes(b.try_into().unwrap());
                // The 8-byte layout is historical; the schema narrows. A stored
                // float4 was rounded to f32 at write, so this cast is lossless.
                out[i] = if matches!(schema[i], ColType::Float4) {
                    Datum::Float4(x as f32)
                } else {
                    Datum::Float8(x)
                };
                at += 8;
            }
            ColType::Text | ColType::Varchar | ColType::Bpchar | ColType::Name => {
                let b = bytes.get(at..at + 4).ok_or_else(corrupt)?;
                let len = u32::from_le_bytes(b.try_into().unwrap()) as usize;
                at += 4;
                let raw = bytes.get(at..at + len).ok_or_else(corrupt)?;
                at += len;
                let s = core::str::from_utf8(raw).map_err(|_| corrupt())?;
                // A char(n) value is stored blank-padded, and in PostgreSQL the
                // padding is part of the value — `max(c)` returns it padded
                // even under typmod -1 — so it decodes into the variant that
                // knows it: comparisons and text casts strip, output does not.
                out[i] = if matches!(schema[i], ColType::Bpchar) {
                    Datum::Bpchar(s)
                } else {
                    Datum::Text(s)
                };
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
            ColType::Timetz => {
                let b = bytes.get(at..at + 12).ok_or_else(corrupt)?;
                out[i] = Datum::Timetz(
                    i64::from_le_bytes(b[..8].try_into().unwrap()),
                    i32::from_le_bytes(b[8..].try_into().unwrap()),
                );
                at += 12;
            }
            ColType::Time => {
                let b = bytes.get(at..at + 8).ok_or_else(corrupt)?;
                out[i] = Datum::Time(i64::from_le_bytes(b.try_into().unwrap()));
                at += 8;
            }
            ColType::Array(element) => {
                let b = bytes.get(at..at + 4).ok_or_else(corrupt)?;
                let payload = u32::from_le_bytes(b.try_into().unwrap()) as usize;
                at += 4;
                // Skip the element-type code byte; the schema is authoritative.
                let raw = bytes.get(at + 1..at + payload).ok_or_else(corrupt)?;
                at += payload;
                out[i] = Datum::Array { element, raw };
            }
            ColType::Json | ColType::Jsonb => {
                let b = bytes.get(at..at + 4).ok_or_else(corrupt)?;
                let len = u32::from_le_bytes(b.try_into().unwrap()) as usize;
                at += 4;
                let raw = bytes.get(at..at + len).ok_or_else(corrupt)?;
                at += len;
                let s = core::str::from_utf8(raw).map_err(|_| corrupt())?;
                out[i] = Datum::Json {
                    text: s,
                    jsonb: matches!(schema[i], ColType::Jsonb),
                };
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
            ColType::Multirange(kind) => {
                let b = bytes.get(at..at + 4).ok_or_else(corrupt)?;
                let len = u32::from_le_bytes(b.try_into().unwrap()) as usize;
                at += 4;
                let raw = bytes.get(at..at + len).ok_or_else(corrupt)?;
                at += len;
                let s = core::str::from_utf8(raw).map_err(|_| corrupt())?;
                out[i] = Datum::Multirange { text: s, kind };
            }
            // Records are transient (DDL refuses them as stored columns), so
            // a record column in the storage row schema is corruption.
            ColType::Record => return Err(corrupt()),
            ColType::Bit { varying } => {
                let b = bytes.get(at..at + 4).ok_or_else(corrupt)?;
                let payload = u32::from_le_bytes(b.try_into().unwrap()) as usize;
                at += 4;
                if payload == 0 {
                    return Err(corrupt());
                }
                // First payload byte is the stored varying flag; the schema is
                // authoritative for the column's declared type, so ignore it.
                let raw = bytes.get(at + 1..at + payload).ok_or_else(corrupt)?;
                at += payload;
                let s = core::str::from_utf8(raw).map_err(|_| corrupt())?;
                out[i] = Datum::Bit { bits: s, varying };
            }
            ColType::Interval => {
                let month = bytes.get(at..at + 4).ok_or_else(corrupt)?;
                let dy = bytes.get(at + 4..at + 8).ok_or_else(corrupt)?;
                let us = bytes.get(at + 8..at + 16).ok_or_else(corrupt)?;
                out[i] = Datum::Interval(crate::sql::types::Interval {
                    months: i32::from_le_bytes(month.try_into().unwrap()),
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
            ColType::Inet | ColType::Cidr => {
                let b = bytes.get(at..at + 18).ok_or_else(corrupt)?;
                out[i] = if matches!(schema[i], ColType::Cidr) {
                    Datum::Cidr(
                        crate::sql::net::NetAddr::new_cidr(
                            b[0],
                            b[1],
                            b[2..18].try_into().unwrap(),
                        )
                        .ok_or_else(corrupt)?,
                    )
                } else {
                    Datum::Inet(
                        crate::sql::net::NetAddr::new(b[0], b[1], b[2..18].try_into().unwrap())
                            .ok_or_else(corrupt)?,
                    )
                };
                at += 18;
            }
            ColType::Macaddr => {
                let b = bytes.get(at..at + 6).ok_or_else(corrupt)?;
                out[i] = Datum::Macaddr(b.try_into().unwrap());
                at += 6;
            }
            ColType::Macaddr8 => {
                let b = bytes.get(at..at + 8).ok_or_else(corrupt)?;
                out[i] = Datum::Macaddr8(b.try_into().unwrap());
                at += 8;
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
                    sign,
                    weight,
                    dscale,
                    digits: raw,
                });
            }
            ColType::Enum(slot) => {
                let b = bytes.get(at..at + 8).ok_or_else(corrupt)?;
                let sort = f64::from_le_bytes(b.try_into().unwrap());
                at += 8;
                let lb = bytes.get(at..at + 4).ok_or_else(corrupt)?;
                let len = u32::from_le_bytes(lb.try_into().unwrap()) as usize;
                at += 4;
                let raw = bytes.get(at..at + len).ok_or_else(corrupt)?;
                at += len;
                let label = core::str::from_utf8(raw).map_err(|_| corrupt())?;
                out[i] = Datum::Enum { slot, sort, label };
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
        let mut buffer = vec![0u8; encoded_len(&values)];
        encode(&values, &mut buffer);
        let mut out = [Datum::Null; MAX_COLUMNS];
        decode(&buffer, &schema, &mut out).unwrap();
        assert_eq!(&out[..6], &values);
    }

    #[test]
    fn truncated_bytes_are_an_error_not_a_panic() {
        let schema = [ColType::Int8];
        let values = [Datum::Int8(1)];
        let mut buffer = vec![0u8; encoded_len(&values)];
        encode(&values, &mut buffer);
        for cut in 0..buffer.len() {
            let mut out = [Datum::Null; 1];
            assert!(
                decode(&buffer[..cut], &schema, &mut out).is_err(),
                "cut={cut}"
            );
        }
    }

    #[test]
    fn schema_mismatch_is_an_error() {
        let values = [Datum::Int4(1)];
        let mut buffer = vec![0u8; encoded_len(&values)];
        encode(&values, &mut buffer);
        let mut out = [Datum::Null; 2];
        assert!(decode(&buffer, &[ColType::Int4, ColType::Int4], &mut out).is_err());
    }

    #[test]
    fn physical_columns_reassemble_into_one_decodable_row() {
        let values = [Datum::Int4(7), Datum::Text("selected")];
        let schema = [ColType::Int4, ColType::Text];
        let mut row = vec![0; encoded_len(&values)];
        encode(&values, &mut row);
        let mut payloads = [&[][..]; MAX_COLUMNS];
        let mut nulls = [false; MAX_COLUMNS];
        encoded_columns(&row, &schema, &mut payloads, &mut nulls).unwrap();
        let mut reassembled = vec![0; row.len()];
        reassembled[..2].copy_from_slice(&(schema.len() as u16).to_le_bytes());
        let mut at = 2 + schema.len().div_ceil(8);
        for column in 0..schema.len() {
            if nulls[column] {
                reassembled[2 + column / 8] |= 1 << (column % 8);
                continue;
            }
            reassembled[at..at + payloads[column].len()].copy_from_slice(payloads[column]);
            at += payloads[column].len();
        }
        let mut decoded = [Datum::Null; 2];
        decode(&reassembled, &schema, &mut decoded).unwrap();
        assert_eq!(decoded, values);
    }
}
