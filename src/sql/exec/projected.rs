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
        Datum::Int2(_) => 2,
        Datum::Int4(_) | Datum::Date(_) => 4,
        Datum::Int8(_)
        | Datum::Float8(_)
        | Datum::Timestamp(_)
        | Datum::Timestamptz(_)
        | Datum::Time(_) => 8,
        Datum::Timetz(..) => 12,
        Datum::Interval(_) => 16,
        Datum::Uuid(_) => 16,
        Datum::Text(s) | Datum::Bpchar(s) => 4 + s.len(),
        Datum::Json { text, .. } => 5 + text.len(),
        Datum::Array { raw, .. } => 6 + raw.len(),
        Datum::Bytea(b) => 4 + b.len(),
        Datum::Numeric(nm) => 7 + nm.digits.len(),
        Datum::Range { text, .. } => 5 + text.len(),
        Datum::Bit { bits, .. } => 5 + bits.len(),
        Datum::Multirange { text, .. } => 5 + text.len(),
        // A record stores its rendered text (the arena-free decode returns
        // that, keeping comparators and output unchanged) followed by a
        // structural tail — field names, OIDs, nested tagged values — that
        // [`decode_projected_col_record`] rebuilds into a `Datum::Record`
        // when a consumer needs field access.
        Datum::Record(fields) => {
            let mut n = 4 + record_text_len(v) + 1;
            for f in *fields {
                n += 1 + f.name.len() + 4 + projected_value_len(&f.value);
            }
            n
        }
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
        Datum::Int2(x) => {
            out[0] = 22;
            out[1..3].copy_from_slice(&x.to_le_bytes());
            3
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
        Datum::Bpchar(str_value) => {
            out[0] = 21;
            out[1..5].copy_from_slice(&(str_value.len() as u32).to_le_bytes());
            out[5..5 + str_value.len()].copy_from_slice(str_value.as_bytes());
            5 + str_value.len()
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
        Datum::Record(fields) => {
            use core::fmt::Write as _;
            // A cursor writing Display output straight into `out` after the
            // 5-byte header (tag + u32 text length).
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
            // Structural tail: field count, then per field its name, type
            // OID, and nested tagged value.
            let mut at = 5 + text_len;
            out[at] = fields.len() as u8;
            at += 1;
            for f in *fields {
                out[at] = f.name.len() as u8;
                at += 1;
                out[at..at + f.name.len()].copy_from_slice(f.name.as_bytes());
                at += f.name.len();
                out[at..at + 4].copy_from_slice(&f.type_oid.to_le_bytes());
                at += 4;
                at += write_projected_value(&f.value, &mut out[at..]);
            }
            at
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
            // The arena-free decode returns a record's rendered text — right
            // for comparators and output. Field access goes through
            // [`decode_projected_col_record`], which rebuilds the structure
            // from the tail this arm skips over.
            let len = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
            let s = core::str::from_utf8(&bytes[at + 4..at + 4 + len]).unwrap_or("");
            (Datum::Text(s), 4 + len + record_tail_len(bytes, at + 4 + len))
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
        22 => (
            Datum::Int2(i16::from_le_bytes(bytes[at..at + 2].try_into().unwrap())),
            2,
        ),
        21 => {
            let len =
                u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
            (
                Datum::Bpchar(
                    core::str::from_utf8(&bytes[at + 4..at + 4 + len])
                        .expect("encoded from valid UTF-8"),
                ),
                4 + len,
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
/// Compares two encoded rows' first `width` columns under SQL equality:
/// column bytes compare directly except bpchar values, which compare by their
/// stripped text — cross-width padding must not split a DISTINCT group.
fn cmp_projected_prefix(a: &[u8], b: &[u8], width: usize) -> core::cmp::Ordering {
    let (mut ia, mut ib) = (1usize, 1usize);
    for _ in 0..width {
        let (ta, tb) = (a[ia], b[ib]);
        ia += 1;
        ib += 1;
        let (da, sa) = decode_projected_value(a, ta, ia);
        let (db, sb) = decode_projected_value(b, tb, ib);
        let ord = match (da, db) {
            (Datum::Bpchar(x), Datum::Bpchar(y)) => {
                x.trim_end_matches(' ').cmp(y.trim_end_matches(' '))
            }
            _ => a[ia - 1..ia + sa].cmp(&b[ib - 1..ib + sb]),
        };
        if !ord.is_eq() {
            return ord;
        }
        ia += sa;
        ib += sb;
    }
    core::cmp::Ordering::Equal
}

/// The byte length of a record's structural tail starting at `at` (the field
/// count byte), nested records included.
fn record_tail_len(bytes: &[u8], at: usize) -> usize {
    let mut cursor = at;
    let n_fields = bytes[cursor] as usize;
    cursor += 1;
    for _ in 0..n_fields {
        let name_len = bytes[cursor] as usize;
        cursor += 1 + name_len + 4;
        let tag = bytes[cursor];
        cursor += 1;
        cursor += decode_projected_value(bytes, tag, cursor).1;
    }
    cursor - at
}

/// Reads column `col` of an encoded row like [`decode_projected_pub`], but
/// rebuilds a record column into a structural [`Datum::Record`] (fields
/// arena-allocated, nested records included) instead of its rendered text.
pub fn decode_projected_col_record<'a>(
    bytes: &'a [u8],
    col: usize,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    let mut at = 1usize;
    let mut current = 0usize;
    loop {
        let tag = bytes[at];
        at += 1;
        let (value, size) = decode_projected_value(bytes, tag, at);
        if current == col {
            if tag == 19 {
                let text_len =
                    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
                return decode_record_tail(bytes, at + 4 + text_len, arena);
            }
            return Ok(value);
        }
        at += size;
        current += 1;
    }
}

/// Rebuilds a `Datum::Record` from the structural tail at `at`.
fn decode_record_tail<'a>(
    bytes: &'a [u8],
    at: usize,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    use crate::sql::types::RecordField;
    let mut cursor = at;
    let n_fields = bytes[cursor] as usize;
    cursor += 1;
    let fields = arena
        .alloc_slice_with(n_fields, |_| RecordField {
            name: "",
            type_oid: 0,
            value: Datum::Null,
        })
        .map_err(|_| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "record decode exceeds the statement arena"
            )
        })?;
    for f in fields.iter_mut() {
        let name_len = bytes[cursor] as usize;
        cursor += 1;
        f.name = core::str::from_utf8(&bytes[cursor..cursor + name_len]).unwrap_or("");
        cursor += name_len;
        f.type_oid = i32::from_le_bytes(bytes[cursor..cursor + 4].try_into().unwrap());
        cursor += 4;
        let tag = bytes[cursor];
        cursor += 1;
        if tag == 19 {
            let text_len =
                u32::from_le_bytes(bytes[cursor..cursor + 4].try_into().unwrap()) as usize;
            f.value = decode_record_tail(bytes, cursor + 4 + text_len, arena)?;
            cursor += decode_projected_value(bytes, tag, cursor).1;
        } else {
            let (value, size) = decode_projected_value(bytes, tag, cursor);
            f.value = value;
            cursor += size;
        }
    }
    Ok(Datum::Record(fields))
}

/// DISTINCT over encoded rows: sorts (grouping SQL-equal rows adjacently,
/// byte order as the tiebreak so the surviving representative is
/// deterministic) and keeps the first of each run. Returns the live count.
pub fn sort_dedup_projected(rows: &mut [&[u8]], width: usize) -> usize {
    rows.sort_unstable_by(|a, b| cmp_projected_prefix(a, b, width).then_with(|| a.cmp(b)));
    let mut unique = 0usize;
    for i in 0..rows.len() {
        let same =
            i > 0 && cmp_projected_prefix(rows[i], rows[unique - 1], width).is_eq();
        if !same {
            rows[unique] = rows[i];
            unique += 1;
        }
    }
    unique
}
