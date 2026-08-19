//! The self-describing row encoding used between query phases.
//!
//! Sorting, deduplicating and paging all want rows as bytes: comparable,
//! copyable, and free of the arena lifetimes a `Datum` carries. Each value is
//! written with a tag for its type and a length where it needs one, so a row
//! can be decoded column by column without consulting the schema that produced
//! it — which is what lets a materialized result outlive the scope it came from.

use crate::mem::arena::Arena;
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql::types::Datum;
use crate::sql_err;

/// Tagged, order-preserving-for-equality encoding of a projected row:
/// per value, a tag byte plus a fixed or length-prefixed payload.
pub fn encode_projected_pub<'a>(values: &[Datum], arena: &'a Arena) -> Result<&'a [u8], SqlError> {
    let len = projected_row_len(values)?;
    let out = arena.alloc_slice_with(len, |_| 0u8).map_err(|_| {
        sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "DISTINCT row exceeds the statement arena"
        )
    })?;
    encode_projected_into(values, out)?;
    Ok(&*out)
}

/// Encodes a projected row whose values are available by index.
///
/// The accessor is called twice: once to size the exact arena allocation and
/// once to encode it. It must therefore only retrieve already-evaluated
/// values; expression evaluation belongs before this encoding boundary.
pub(crate) fn encode_projected_by<'a>(
    count: usize,
    mut value_at: impl FnMut(usize) -> Datum<'a>,
    arena: &'a Arena,
) -> Result<&'a [u8], SqlError> {
    let len = projected_row_len_by(count, &mut value_at)?;
    let out = arena.alloc_slice_with(len, |_| 0u8).map_err(|_| {
        sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "DISTINCT row exceeds the statement arena"
        )
    })?;
    encode_projected_by_into(count, &mut value_at, out)?;
    Ok(&*out)
}

/// Exact byte length of a projected row whose values are read by index.
pub(crate) fn projected_row_len_by<'a>(
    count: usize,
    mut value_at: impl FnMut(usize) -> Datum<'a>,
) -> Result<usize, SqlError> {
    let count_u16 = u16::try_from(count).map_err(|_| {
        sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "projected row has too many columns"
        )
    })?;
    let mut len = size_of_val(&count_u16);
    for index in 0..count {
        len = len
            .checked_add(projected_value_len(&value_at(index)))
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "projected row is too large"
                )
            })?;
    }
    Ok(len)
}

/// Writes an indexed projected row into caller-owned external-run storage.
pub(crate) fn encode_projected_by_into<'a>(
    count: usize,
    mut value_at: impl FnMut(usize) -> Datum<'a>,
    out: &mut [u8],
) -> Result<usize, SqlError> {
    let len = projected_row_len_by(count, &mut value_at)?;
    if out.len() < len {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "projected row of {} bytes exceeds external-run block capacity {}",
            len,
            out.len()
        ));
    }
    let count_u16 = u16::try_from(count).expect("projected_row_len_by checked the count");
    out[..2].copy_from_slice(&count_u16.to_le_bytes());
    let mut at = 2usize;
    for index in 0..count {
        at += write_projected_value(&value_at(index), &mut out[at..]);
    }
    debug_assert_eq!(at, len);
    Ok(at)
}

/// Exact byte length needed by [`encode_projected_into`].
pub(crate) fn projected_row_len(values: &[Datum]) -> Result<usize, SqlError> {
    let count = u16::try_from(values.len()).map_err(|_| {
        sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "projected row has too many columns"
        )
    })?;
    let mut len = size_of_val(&count);
    for v in values {
        len = len.checked_add(projected_value_len(v)).ok_or_else(|| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "projected row is too large"
            )
        })?;
    }
    Ok(len)
}

/// Encodes one projected row into caller-owned storage.
///
/// The arena path and the external-run path share this writer, so the two
/// representations cannot drift. `out` may be larger than necessary; the
/// returned length names the initialized prefix.
pub(crate) fn encode_projected_into(values: &[Datum], out: &mut [u8]) -> Result<usize, SqlError> {
    let len = projected_row_len(values)?;
    if out.len() < len {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "projected row of {} bytes exceeds external-run block capacity {}",
            len,
            out.len()
        ));
    }
    let count = u16::try_from(values.len()).expect("projected_row_len checked the count");
    out[..2].copy_from_slice(&count.to_le_bytes());
    let mut at = 2usize;
    for v in values {
        at += write_projected_value(v, &mut out[at..]);
    }
    debug_assert_eq!(at, len);
    Ok(at)
}

/// Number of values stored in a projected row.
pub(crate) fn projected_row_width(bytes: &[u8]) -> usize {
    u16::from_le_bytes(bytes[..2].try_into().expect("projected row header")) as usize
}

/// The projected-encoding byte length of one value (tag + payload).
pub fn projected_value_len(v: &Datum) -> usize {
    1 + match v {
        Datum::Null => 0,
        Datum::Bool(_) => 1,
        Datum::Int2(_) => 2,
        Datum::Float4(_) => 4,
        Datum::Int4(_) | Datum::Oid(_) | Datum::Date(_) => 4,
        Datum::Int8(_)
        | Datum::Float8(_)
        | Datum::Timestamp(_)
        | Datum::Timestamptz(_)
        | Datum::Time(_) => 8,
        Datum::Timetz(..) => 12,
        Datum::Interval(_) => 16,
        Datum::Uuid(_) => 16,
        Datum::Inet(_) | Datum::Cidr(_) => 18,
        Datum::Macaddr(_) => 6,
        Datum::Macaddr8(_) => 8,
        Datum::Text(s) | Datum::Bpchar(s) => 4 + s.len(),
        Datum::Regtype { name, .. } => 8 + name.len(),
        Datum::RegObject { name, .. } => 12 + name.len(),
        Datum::Json { text, .. } => 5 + text.len(),
        Datum::Array { raw, .. } => 8 + raw.len(),
        Datum::Int2Vector(raw) => 4 + raw.len(),
        Datum::Bytea(b) => 4 + b.len(),
        Datum::Numeric(nm) => 7 + nm.digits.len(),
        Datum::Range { text, .. } => 5 + text.len(),
        Datum::Bit { bits, .. } => 5 + bits.len(),
        Datum::Multirange { text, .. } => 5 + text.len(),
        // Projected values are schema-less, so an enum carries its slot too:
        // slot(2) + sort(8) + 4-byte label length + label bytes.
        Datum::Enum { label, .. } => 14 + label.len(),
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
        Datum::Composite { fields, .. } => {
            let mut n = 2 + 4 + record_text_len(v) + 1;
            for f in *fields {
                n += 1 + f.name.len() + 4 + projected_value_len(&f.value);
            }
            n
        }
        Datum::CompositeText { text, .. } => 2 + 1 + 4 + text.len(),
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
        Datum::Oid(x) => {
            out[0] = 35;
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
        Datum::Float4(x) => {
            out[0] = 23;
            out[1..5].copy_from_slice(&x.to_bits().to_le_bytes());
            5
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
        Datum::Regtype {
            referenced_oid,
            name,
        } => {
            out[0] = 30;
            out[1..5].copy_from_slice(&referenced_oid.to_le_bytes());
            out[5..9].copy_from_slice(&(name.len() as u32).to_le_bytes());
            out[9..9 + name.len()].copy_from_slice(name.as_bytes());
            9 + name.len()
        }
        Datum::RegObject {
            type_oid,
            referenced_oid,
            name,
        } => {
            out[0] = 31;
            out[1..5].copy_from_slice(&type_oid.to_le_bytes());
            out[5..9].copy_from_slice(&referenced_oid.to_le_bytes());
            out[9..13].copy_from_slice(&(name.len() as u32).to_le_bytes());
            out[13..13 + name.len()].copy_from_slice(name.as_bytes());
            13 + name.len()
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
            let (base_code, base_user_slot) = match element {
                crate::sql::types::ArrElem::Domain {
                    base_code,
                    base_user_slot,
                    ..
                } => (*base_code, *base_user_slot),
                _ => (0, crate::sql::types::ColType::ENUM_SLOT_UNRESOLVED),
            };
            out[2] = base_code;
            out[3..5].copy_from_slice(&base_user_slot.to_le_bytes());
            out[5..9].copy_from_slice(&(raw.len() as u32).to_le_bytes());
            out[9..9 + raw.len()].copy_from_slice(raw);
            9 + raw.len()
        }
        Datum::Int2Vector(raw) => {
            out[0] = 29;
            out[1..5].copy_from_slice(&(raw.len() as u32).to_le_bytes());
            out[5..5 + raw.len()].copy_from_slice(raw);
            5 + raw.len()
        }
        Datum::Uuid(b) => {
            out[0] = 9;
            out[1..17].copy_from_slice(b);
            17
        }
        Datum::Inet(net) | Datum::Cidr(net) => {
            out[0] = if matches!(v, Datum::Cidr(_)) { 25 } else { 24 };
            out[1] = net.family();
            out[2] = net.bits();
            out[3..19].copy_from_slice(net.addr());
            19
        }
        Datum::Macaddr(b) => {
            out[0] = 26;
            out[1..7].copy_from_slice(b);
            7
        }
        Datum::Macaddr8(b) => {
            out[0] = 27;
            out[1..9].copy_from_slice(b);
            9
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
        Datum::Enum { slot, sort, label } => {
            out[0] = 28;
            out[1..3].copy_from_slice(&slot.to_le_bytes());
            out[3..11].copy_from_slice(&sort.to_le_bytes());
            out[11..15].copy_from_slice(&(label.len() as u32).to_le_bytes());
            out[15..15 + label.len()].copy_from_slice(label.as_bytes());
            15 + label.len()
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
        Datum::Composite { slot, fields } => {
            use core::fmt::Write as _;
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
            out[0] = 32;
            out[1..3].copy_from_slice(&slot.to_le_bytes());
            let mut w = SliceWriter { buf: out, at: 7 };
            let _ = write!(w, "{v}");
            let text_len = w.at - 7;
            out[3..7].copy_from_slice(&(text_len as u32).to_le_bytes());
            let mut at = 7 + text_len;
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
        Datum::CompositeText {
            slot,
            physical_fields,
            text,
        } => {
            out[0] = 33;
            out[1..3].copy_from_slice(&slot.to_le_bytes());
            out[3] = *physical_fields;
            out[4..8].copy_from_slice(&(text.len() as u32).to_le_bytes());
            out[8..8 + text.len()].copy_from_slice(text.as_bytes());
            8 + text.len()
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
        35 => (
            Datum::Oid(u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())),
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
        23 => (
            Datum::Float4(f32::from_bits(u32::from_le_bytes(
                bytes[at..at + 4].try_into().unwrap(),
            ))),
            4,
        ),
        5 => {
            let len = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
            (
                Datum::Text(
                    core::str::from_utf8(&bytes[at + 4..at + 4 + len])
                        .expect("encoded from valid UTF-8"),
                ),
                4 + len,
            )
        }
        30 => {
            let referenced_oid = i32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
            let len = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap()) as usize;
            (
                Datum::Regtype {
                    referenced_oid,
                    name: core::str::from_utf8(&bytes[at + 8..at + 8 + len])
                        .expect("encoded from valid UTF-8"),
                },
                8 + len,
            )
        }
        31 => {
            let type_oid = i32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
            let referenced_oid = i32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap());
            let len = u32::from_le_bytes(bytes[at + 8..at + 12].try_into().unwrap()) as usize;
            (
                Datum::RegObject {
                    type_oid,
                    referenced_oid,
                    name: core::str::from_utf8(&bytes[at + 12..at + 12 + len])
                        .expect("encoded from valid UTF-8"),
                },
                12 + len,
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
            let s = core::str::from_utf8(&bytes[at + 5..at + 5 + len])
                .expect("projected JSON was encoded from valid UTF-8");
            (Datum::Json { text: s, jsonb }, 5 + len)
        }
        15 => {
            let mut element = crate::sql::types::ArrElem::from_code(bytes[at])
                .expect("projected array carries a valid element code");
            if let crate::sql::types::ArrElem::Domain { slot, .. } = element {
                element = crate::sql::types::ArrElem::Domain {
                    slot,
                    base_code: bytes[at + 1],
                    base_user_slot: u16::from_le_bytes(bytes[at + 2..at + 4].try_into().unwrap()),
                };
            }
            let len = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap()) as usize;
            (
                Datum::Array {
                    element,
                    raw: &bytes[at + 8..at + 8 + len],
                },
                8 + len,
            )
        }
        16 => {
            let kind = crate::sql::types::RangeKind::from_code(bytes[at])
                .expect("projected range carries a valid kind code");
            let len = u32::from_le_bytes(bytes[at + 1..at + 5].try_into().unwrap()) as usize;
            let s = core::str::from_utf8(&bytes[at + 5..at + 5 + len])
                .expect("projected range was encoded from valid UTF-8");
            (Datum::Range { text: s, kind }, 5 + len)
        }
        17 => {
            let varying = bytes[at] != 0;
            let len = u32::from_le_bytes(bytes[at + 1..at + 5].try_into().unwrap()) as usize;
            let s = core::str::from_utf8(&bytes[at + 5..at + 5 + len])
                .expect("projected bit string was encoded from valid UTF-8");
            (Datum::Bit { bits: s, varying }, 5 + len)
        }
        18 => {
            let kind = crate::sql::types::RangeKind::from_code(bytes[at])
                .expect("projected multirange carries a valid kind code");
            let len = u32::from_le_bytes(bytes[at + 1..at + 5].try_into().unwrap()) as usize;
            let s = core::str::from_utf8(&bytes[at + 5..at + 5 + len])
                .expect("projected multirange was encoded from valid UTF-8");
            (Datum::Multirange { text: s, kind }, 5 + len)
        }
        19 => {
            // The arena-free decode returns a record's rendered text — right
            // for comparators and output. Field access goes through
            // [`decode_projected_col_record`], which rebuilds the structure
            // from the tail this arm skips over.
            let len = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
            let s = core::str::from_utf8(&bytes[at + 4..at + 4 + len])
                .expect("projected record text was encoded from valid UTF-8");
            (
                Datum::Text(s),
                4 + len + record_tail_len(bytes, at + 4 + len),
            )
        }
        9 => (Datum::Uuid(bytes[at..at + 16].try_into().unwrap()), 16),
        10 => {
            let len = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
            (Datum::Bytea(&bytes[at + 4..at + 4 + len]), 4 + len)
        }
        11 => {
            let sign = match bytes[at] {
                0 => crate::sql::numeric::Sign::Pos,
                1 => crate::sql::numeric::Sign::Neg,
                2 => crate::sql::numeric::Sign::NaN,
                _ => panic!("projected numeric carries a valid sign tag"),
            };
            let weight = i16::from_le_bytes(bytes[at + 1..at + 3].try_into().unwrap());
            let dscale = u16::from_le_bytes(bytes[at + 3..at + 5].try_into().unwrap());
            let ndigits = u16::from_le_bytes(bytes[at + 5..at + 7].try_into().unwrap()) as usize;
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
            let len = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
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
        24 | 25 => {
            let d = if tag == 25 {
                Datum::Cidr(
                    crate::sql::net::NetAddr::new_cidr(
                        bytes[at],
                        bytes[at + 1],
                        bytes[at + 2..at + 18]
                            .try_into()
                            .expect("fixed network encoding"),
                    )
                    .expect("encoded cidr address is valid"),
                )
            } else {
                Datum::Inet(
                    crate::sql::net::NetAddr::new(
                        bytes[at],
                        bytes[at + 1],
                        bytes[at + 2..at + 18]
                            .try_into()
                            .expect("fixed network encoding"),
                    )
                    .expect("encoded inet address is valid"),
                )
            };
            (d, 18)
        }
        26 => (Datum::Macaddr(bytes[at..at + 6].try_into().unwrap()), 6),
        27 => (Datum::Macaddr8(bytes[at..at + 8].try_into().unwrap()), 8),
        28 => {
            let slot = u16::from_le_bytes(bytes[at..at + 2].try_into().unwrap());
            let sort = f64::from_le_bytes(bytes[at + 2..at + 10].try_into().unwrap());
            let len = u32::from_le_bytes(bytes[at + 10..at + 14].try_into().unwrap()) as usize;
            let label = core::str::from_utf8(&bytes[at + 14..at + 14 + len])
                .expect("projected enum label is valid UTF-8");
            (Datum::Enum { slot, sort, label }, 14 + len)
        }
        29 => {
            let len = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
            (Datum::Int2Vector(&bytes[at + 4..at + 4 + len]), 4 + len)
        }
        32 => {
            let len = u32::from_le_bytes(bytes[at + 2..at + 6].try_into().unwrap()) as usize;
            let s = core::str::from_utf8(&bytes[at + 6..at + 6 + len])
                .expect("projected composite text was encoded from valid UTF-8");
            (
                Datum::Text(s),
                6 + len + record_tail_len(bytes, at + 6 + len),
            )
        }
        33 => {
            let slot = u16::from_le_bytes(bytes[at..at + 2].try_into().unwrap());
            let physical_fields = bytes[at + 2];
            let len = u32::from_le_bytes(bytes[at + 3..at + 7].try_into().unwrap()) as usize;
            let text = core::str::from_utf8(&bytes[at + 7..at + 7 + len])
                .expect("projected composite text was encoded from valid UTF-8");
            (
                Datum::CompositeText {
                    slot,
                    physical_fields,
                    text,
                },
                7 + len,
            )
        }
        _ => unreachable!("tags are exhaustive"),
    }
}

/// Byte length of an encoded row's first `width` values, tags included.
pub fn projected_prefix_len(bytes: &[u8], width: usize) -> usize {
    let mut at = 2usize;
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
    let mut at = 2usize;
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
pub(crate) fn compare_projected_prefix(a: &[u8], b: &[u8], width: usize) -> core::cmp::Ordering {
    let (mut ia, mut ib) = (2usize, 2usize);
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
    let mut at = 2usize;
    let mut current = 0usize;
    loop {
        let tag = bytes[at];
        at += 1;
        let (value, size) = decode_projected_value(bytes, tag, at);
        if current == col {
            if tag == 19 {
                let text_len = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
                return decode_record_tail(bytes, at + 4 + text_len, arena);
            }
            if tag == 32 {
                let slot = u16::from_le_bytes(bytes[at..at + 2].try_into().unwrap());
                let text_len =
                    u32::from_le_bytes(bytes[at + 2..at + 6].try_into().unwrap()) as usize;
                let Datum::Record(fields) = decode_record_tail(bytes, at + 6 + text_len, arena)?
                else {
                    unreachable!()
                };
                return Ok(Datum::Composite { slot, fields });
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
        f.name = core::str::from_utf8(&bytes[cursor..cursor + name_len])
            .expect("projected record name was encoded from valid UTF-8");
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
        } else if tag == 32 {
            let slot = u16::from_le_bytes(bytes[cursor..cursor + 2].try_into().unwrap());
            let text_len =
                u32::from_le_bytes(bytes[cursor + 2..cursor + 6].try_into().unwrap()) as usize;
            let Datum::Record(fields) = decode_record_tail(bytes, cursor + 6 + text_len, arena)?
            else {
                unreachable!()
            };
            f.value = Datum::Composite { slot, fields };
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
    rows.sort_unstable_by(|a, b| compare_projected_prefix(a, b, width).then_with(|| a.cmp(b)));
    let mut unique = 0usize;
    for i in 0..rows.len() {
        let same = i > 0 && compare_projected_prefix(rows[i], rows[unique - 1], width).is_eq();
        if !same {
            rows[unique] = rows[i];
            unique += 1;
        }
    }
    unique
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem::budget::Budget;

    #[test]
    fn projected_width_is_not_truncated_to_one_byte() {
        let mut budget = Budget::new(64 * 1024);
        let arena = Arena::new(&mut budget, "wide projected row", 32 * 1024).unwrap();
        let values = [Datum::Null; 300];
        let encoded = encode_projected_pub(&values, &arena).unwrap();
        assert_eq!(projected_row_width(encoded), values.len());
        assert!(decode_projected_pub(encoded, values.len() - 1).is_null());
    }

    #[test]
    fn structural_decode_preserves_record_fields() {
        use crate::sql::types::{RecordField, oid};

        let mut budget = Budget::new(64 * 1024);
        let arena = Arena::new(&mut budget, "record projected row", 32 * 1024).unwrap();
        let fields = [
            RecordField {
                name: "f1",
                type_oid: oid::INT4,
                value: Datum::Int4(42),
            },
            RecordField {
                name: "f2",
                type_oid: oid::TEXT,
                value: Datum::Null,
            },
        ];
        let encoded = encode_projected_pub(&[Datum::Record(&fields)], &arena).unwrap();

        assert_eq!(decode_projected_pub(encoded, 0), Datum::Text("(42,)"));
        assert_eq!(
            decode_projected_col_record(encoded, 0, &arena).unwrap(),
            Datum::Record(&fields)
        );
    }
}
