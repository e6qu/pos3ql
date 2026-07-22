//! The self-describing row encoding used between query phases.
//!
//! Sorting, deduplicating and paging all want rows as bytes: comparable,
//! copyable, and free of the arena lifetimes a `Datum` carries. Each value is
//! written with a tag for its type and a length where it needs one, so a row
//! can be decoded column by column without consulting the schema that produced
//! it — which is what lets a materialized result outlive the scope it came from.

use crate::mem::arena::Arena;
use crate::sql::eval::{sqlstate, SqlError};
use crate::sql::types::Datum;
use crate::sql_err;

/// Tagged, order-preserving-for-equality encoding of a projected row:
/// per value, a tag byte plus a fixed or length-prefixed payload.
pub fn encode_projected_pub<'a>(values: &[Datum], arena: &'a Arena) -> Result<&'a [u8], SqlError> {
    let mut len = 1usize;
    for v in values {
        len += projected_value_len(v);
    }
    let out = arena.alloc_slice_with(len, |_| 0u8).map_err(|_| {
        sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "DISTINCT row exceeds the statement arena"
        )
    })?;
    out[0] = values.len() as u8;
    let mut at = 1usize;
    for v in values {
        at += write_projected_value(v, &mut out[at..]);
    }
    Ok(&*out)
}

/// The projected-encoding byte length of one value (tag + payload).
pub fn projected_value_len(v: &Datum) -> usize {
    1 + match v {
        Datum::Null => 0,
        Datum::Bool(_) => 1,
        Datum::Int4(_) | Datum::Date(_) => 4,
        Datum::Int8(_)
        | Datum::Float8(_)
        | Datum::Timestamp(_)
        | Datum::Timestamptz(_)
        | Datum::Time(_) => 8,
        Datum::Timetz(..) => 12,
        Datum::Interval(_) => 16,
        Datum::Uuid(_) => 16,
        Datum::Text(s) => 4 + s.len(),
        Datum::Json { text, .. } => 5 + text.len(),
        Datum::Array { raw, .. } => 6 + raw.len(),
        Datum::Bytea(b) => 4 + b.len(),
        Datum::Numeric(nm) => 7 + nm.digits.len(),
        Datum::Range { text, .. } => 5 + text.len(),
        Datum::Bit { bits, .. } => 5 + bits.len(),
        Datum::Multirange { text, .. } => 5 + text.len(),
        // A record is stored as its rendered text (decode has no arena to
        // rebuild the field slice); the column's RECORD type comes from the
        // describe pass, so output is unaffected.
        Datum::Record(_) => 4 + record_text_len(v),
    }
}

/// The byte length of a value's `Display` output (no allocation).
fn record_text_len(v: &Datum) -> usize {
    use core::fmt::Write as _;
    struct Counter(usize);
    impl core::fmt::Write for Counter {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            self.0 += s.len();
            Ok(())
        }
    }
    let mut c = Counter(0);
    let _ = write!(c, "{v}");
    c.0
}

/// Writes one value's tag+payload into `out[0..]` (already sized by
/// `projected_value_len`), returning the bytes written. Shared by the
/// top-level encoder and a record's nested fields.
fn write_projected_value(v: &Datum, out: &mut [u8]) -> usize {
    match v {
        Datum::Null => {
            out[0] = 0;
            1
        }
        Datum::Bool(b) => {
            out[0] = 1;
            out[1] = u8::from(*b);
            2
        }
        Datum::Int4(x) => {
            out[0] = 2;
            out[1..5].copy_from_slice(&x.to_le_bytes());
            5
        }
        Datum::Int8(x) => {
            out[0] = 3;
            out[1..9].copy_from_slice(&x.to_le_bytes());
            9
        }
        Datum::Float8(x) => {
            out[0] = 4;
            out[1..9].copy_from_slice(&x.to_bits().to_le_bytes());
            9
        }
        Datum::Text(str_value) => {
            out[0] = 5;
            out[1..5].copy_from_slice(&(str_value.len() as u32).to_le_bytes());
            out[5..5 + str_value.len()].copy_from_slice(str_value.as_bytes());
            5 + str_value.len()
        }
        Datum::Date(x) => {
            out[0] = 6;
            out[1..5].copy_from_slice(&x.to_le_bytes());
            5
        }
        Datum::Timestamp(x) => {
            out[0] = 7;
            out[1..9].copy_from_slice(&x.to_le_bytes());
            9
        }
        Datum::Timestamptz(x) => {
            out[0] = 8;
            out[1..9].copy_from_slice(&x.to_le_bytes());
            9
        }
        Datum::Time(x) => {
            out[0] = 12;
            out[1..9].copy_from_slice(&x.to_le_bytes());
            9
        }
        Datum::Timetz(x, zone) => {
            out[0] = 20;
            out[1..9].copy_from_slice(&x.to_le_bytes());
            out[9..13].copy_from_slice(&zone.to_le_bytes());
            13
        }
        Datum::Interval(interval) => {
            out[0] = 13;
            out[1..5].copy_from_slice(&interval.months.to_le_bytes());
            out[5..9].copy_from_slice(&interval.days.to_le_bytes());
            out[9..17].copy_from_slice(&interval.micros.to_le_bytes());
            17
        }
        Datum::Json { text, jsonb } => {
            out[0] = 14;
            out[1] = u8::from(*jsonb);
            out[2..6].copy_from_slice(&(text.len() as u32).to_le_bytes());
            out[6..6 + text.len()].copy_from_slice(text.as_bytes());
            6 + text.len()
        }
        Datum::Array { element, raw } => {
            out[0] = 15;
            out[1] = element.code();
            out[2..6].copy_from_slice(&(raw.len() as u32).to_le_bytes());
            out[6..6 + raw.len()].copy_from_slice(raw);
            6 + raw.len()
        }
        Datum::Uuid(b) => {
            out[0] = 9;
            out[1..17].copy_from_slice(b);
            17
        }
        Datum::Bytea(b) => {
            out[0] = 10;
            out[1..5].copy_from_slice(&(b.len() as u32).to_le_bytes());
            out[5..5 + b.len()].copy_from_slice(b);
            5 + b.len()
        }
        Datum::Numeric(nm) => {
            out[0] = 11;
            out[1] = match nm.sign {
                crate::sql::numeric::Sign::Pos => 0,
                crate::sql::numeric::Sign::Neg => 1,
                crate::sql::numeric::Sign::NaN => 2,
            };
            out[2..4].copy_from_slice(&nm.weight.to_le_bytes());
            out[4..6].copy_from_slice(&nm.dscale.to_le_bytes());
            out[6..8].copy_from_slice(&(nm.ndigits() as u16).to_le_bytes());
            out[8..8 + nm.digits.len()].copy_from_slice(nm.digits);
            8 + nm.digits.len()
        }
        Datum::Range { text, kind } => {
            out[0] = 16;
            out[1] = kind.code();
            out[2..6].copy_from_slice(&(text.len() as u32).to_le_bytes());
            out[6..6 + text.len()].copy_from_slice(text.as_bytes());
            6 + text.len()
        }
        Datum::Bit { bits, varying } => {
            out[0] = 17;
            out[1] = u8::from(*varying);
            out[2..6].copy_from_slice(&(bits.len() as u32).to_le_bytes());
            out[6..6 + bits.len()].copy_from_slice(bits.as_bytes());
            6 + bits.len()
        }
        Datum::Multirange { text, kind } => {
            out[0] = 18;
            out[1] = kind.code();
            out[2..6].copy_from_slice(&(text.len() as u32).to_le_bytes());
            out[6..6 + text.len()].copy_from_slice(text.as_bytes());
            6 + text.len()
        }
        Datum::Record(_) => {
            use core::fmt::Write as _;
            // A cursor writing Display output straight into `out` after the
            // 5-byte header (tag + u32 length).
            struct SliceWriter<'b> {
                buf: &'b mut [u8],
                at: usize,
            }
            impl core::fmt::Write for SliceWriter<'_> {
                fn write_str(&mut self, s: &str) -> core::fmt::Result {
                    self.buf[self.at..self.at + s.len()].copy_from_slice(s.as_bytes());
                    self.at += s.len();
                    Ok(())
                }
            }
            out[0] = 19;
            let mut w = SliceWriter { buf: out, at: 5 };
            let _ = write!(w, "{v}");
            let text_len = w.at - 5;
            out[1..5].copy_from_slice(&(text_len as u32).to_le_bytes());
            5 + text_len
        }
    }
}

/// Reads the value whose tag is `tag` at byte `at`, returning it and its
/// payload length. This is the one place the projected encoding's tag
/// sizes live: a second, hand-written copy in the sort path drifted from
/// it and panicked the server on every tag it had not been taught.
pub fn decode_projected_value(bytes: &[u8], tag: u8, at: usize) -> (Datum<'_>, usize) {
    match tag {
        0 => (Datum::Null, 0),
        1 => (Datum::Bool(bytes[at] != 0), 1),
        2 => (
            Datum::Int4(i32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())),
            4,
        ),
        3 => (
            Datum::Int8(i64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())),
            8,
        ),
        4 => (
            Datum::Float8(f64::from_bits(u64::from_le_bytes(
                bytes[at..at + 8].try_into().unwrap(),
            ))),
            8,
        ),
        5 => {
            let len =
                u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
            (
                Datum::Text(
                    core::str::from_utf8(&bytes[at + 4..at + 4 + len])
                        .expect("encoded from valid UTF-8"),
                ),
                4 + len,
            )
        }
        6 => (
            Datum::Date(i32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())),
            4,
        ),
        7 => (
            Datum::Timestamp(i64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())),
            8,
        ),
        8 => (
            Datum::Timestamptz(i64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())),
            8,
        ),
        12 => (
            Datum::Time(i64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())),
            8,
        ),
        13 => (
            Datum::Interval(crate::sql::types::Interval {
                months: i32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()),
                days: i32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap()),
                micros: i64::from_le_bytes(bytes[at + 8..at + 16].try_into().unwrap()),
            }),
            16,
        ),
        14 => {
            let jsonb = bytes[at] != 0;
            let len = u32::from_le_bytes(bytes[at + 1..at + 5].try_into().unwrap()) as usize;
            let s = core::str::from_utf8(&bytes[at + 5..at + 5 + len]).unwrap_or("");
            (Datum::Json { text: s, jsonb }, 5 + len)
        }
        15 => {
            let element = crate::sql::types::ArrElem::from_code(bytes[at]).unwrap_or(crate::sql::types::ArrElem::Int4);
            let len = u32::from_le_bytes(bytes[at + 1..at + 5].try_into().unwrap()) as usize;
            (Datum::Array { element, raw: &bytes[at + 5..at + 5 + len] }, 5 + len)
        }
        16 => {
            let kind = crate::sql::types::RangeKind::from_code(bytes[at]);
            let len = u32::from_le_bytes(bytes[at + 1..at + 5].try_into().unwrap()) as usize;
            let s = core::str::from_utf8(&bytes[at + 5..at + 5 + len]).unwrap_or("");
            (Datum::Range { text: s, kind }, 5 + len)
        }
        17 => {
            let varying = bytes[at] != 0;
            let len = u32::from_le_bytes(bytes[at + 1..at + 5].try_into().unwrap()) as usize;
            let s = core::str::from_utf8(&bytes[at + 5..at + 5 + len]).unwrap_or("");
            (Datum::Bit { bits: s, varying }, 5 + len)
        }
        18 => {
            let kind = crate::sql::types::RangeKind::from_code(bytes[at]);
            let len = u32::from_le_bytes(bytes[at + 1..at + 5].try_into().unwrap()) as usize;
            let s = core::str::from_utf8(&bytes[at + 5..at + 5 + len]).unwrap_or("");
            (Datum::Multirange { text: s, kind }, 5 + len)
        }
        19 => {
            // A record is stored as its rendered text; the column's RECORD
            // type comes from describe, so returning Text renders it right.
            let len = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
            let s = core::str::from_utf8(&bytes[at + 4..at + 4 + len]).unwrap_or("");
            (Datum::Text(s), 4 + len)
        }
        9 => (Datum::Uuid(bytes[at..at + 16].try_into().unwrap()), 16),
        10 => {
            let len =
                u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
            (Datum::Bytea(&bytes[at + 4..at + 4 + len]), 4 + len)
        }
        11 => {
            let sign = match bytes[at] {
                0 => crate::sql::numeric::Sign::Pos,
                1 => crate::sql::numeric::Sign::Neg,
                _ => crate::sql::numeric::Sign::NaN,
            };
            let weight = i16::from_le_bytes(bytes[at + 1..at + 3].try_into().unwrap());
            let dscale = u16::from_le_bytes(bytes[at + 3..at + 5].try_into().unwrap());
            let ndigits =
                u16::from_le_bytes(bytes[at + 5..at + 7].try_into().unwrap()) as usize;
            (
                Datum::Numeric(crate::sql::numeric::Numeric {
                    sign,
                    weight,
                    dscale,
                    digits: &bytes[at + 7..at + 7 + ndigits * 2],
                }),
                7 + ndigits * 2,
            )
        }
        20 => (
            Datum::Timetz(
                i64::from_le_bytes(bytes[at..at + 8].try_into().unwrap()),
                i32::from_le_bytes(bytes[at + 8..at + 12].try_into().unwrap()),
            ),
            12,
        ),
        _ => unreachable!("tags are exhaustive"),
    }
}

/// Byte length of an encoded row's first `width` values, tags included.
pub fn projected_prefix_len(bytes: &[u8], width: usize) -> usize {
    let mut at = 1usize;
    for _ in 0..width {
        let tag = bytes[at];
        // The reader takes the offset *past* the tag, as its own caller does.
        at += 1;
        at += decode_projected_value(bytes, tag, at).1;
    }
    at
}

/// Reads column `col` back out of an [`encode_projected`] row.
pub fn decode_projected_pub(bytes: &[u8], col: usize) -> Datum<'_> {
    let mut at = 1usize;
    let mut current = 0usize;
    loop {
        let tag = bytes[at];
        at += 1;
        let (value, size) = decode_projected_value(bytes, tag, at);
        if current == col {
            return value;
        }
        at += size;
        current += 1;
    }
}