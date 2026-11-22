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
use crate::storage::rowenc::StoredForm;

/// Tagged, order-preserving-for-equality encoding of a projected row:
/// per value, a tag byte plus a fixed or length-prefixed payload.
pub fn encode_projected_pub<'a>(values: &[Datum], arena: &'a Arena) -> Result<&'a [u8], SqlError> {
    encode_projected_formed(values, &[], arena)
}

/// [`encode_projected_pub`] of values passed on from stored rows in
/// `forms` (`Inline` past its end): each value keeps the TOAST form it was
/// stored in, as PostgreSQL passes the stored datum on unchanged.
pub(crate) fn encode_projected_formed<'a>(
    values: &[Datum],
    forms: &[StoredForm],
    arena: &'a Arena,
) -> Result<&'a [u8], SqlError> {
    let len = projected_row_len_formed(values, forms)?;
    let out = arena.alloc_slice_with(len, |_| 0u8).map_err(|_| {
        sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "projected row exceeds the statement arena"
        )
    })?;
    encode_projected_formed_into(values, forms, out)?;
    Ok(&*out)
}

/// The tag of a value that carries the TOAST form of the stored datum it
/// was read from: a [`STORED_FORM_RECORD`]-byte form record, then the value
/// with its own tag. Every reader decodes the value through
/// [`decode_projected_value`], so equality, ordering, hashing and grouping
/// never see the form.
const STORED_FORM_TAG: u8 = 60;
/// A form kind byte and its `u32` size.
const STORED_FORM_RECORD: usize = 5;

fn form_record(form: StoredForm) -> [u8; STORED_FORM_RECORD] {
    let (kind, size) = match form {
        StoredForm::Inline => unreachable!("an inline value carries no form record"),
        StoredForm::Compressed(size) => (1, size),
        StoredForm::External {
            saved,
            compressed: false,
        } => (2, saved),
        StoredForm::External {
            saved,
            compressed: true,
        } => (3, saved),
        StoredForm::Unmodelled => (4, 0),
    };
    let mut record = [kind, 0, 0, 0, 0];
    record[1..].copy_from_slice(&size.to_le_bytes());
    record
}

/// The form a projected value whose tag is at `tag_at` carries.
pub(crate) fn projected_stored_form_at(bytes: &[u8], tag_at: usize) -> StoredForm {
    if bytes[tag_at] != STORED_FORM_TAG {
        return StoredForm::Inline;
    }
    let record = &bytes[tag_at + 1..tag_at + 1 + STORED_FORM_RECORD];
    let size = u32::from_le_bytes(record[1..].try_into().expect("form size"));
    match record[0] {
        1 => StoredForm::Compressed(size),
        2 => StoredForm::External {
            saved: size,
            compressed: false,
        },
        3 => StoredForm::External {
            saved: size,
            compressed: true,
        },
        4 => StoredForm::Unmodelled,
        kind => unreachable!("projected form kind {kind} was never encoded"),
    }
}

/// The form column `col` of a projected row carries.
pub(crate) fn projected_stored_form(bytes: &[u8], col: usize) -> StoredForm {
    projected_stored_form_at(bytes, ProjectedValue::column_tag_at(bytes, col))
}

/// The forms of a projected row's columns into `forms`.
pub(crate) fn projected_stored_forms(bytes: &[u8], forms: &mut [StoredForm]) {
    let mut at = 2usize;
    for form in forms.iter_mut().take(projected_row_width(bytes)) {
        *form = projected_stored_form_at(bytes, at);
        at += 1 + decode_projected_value(bytes, bytes[at], at + 1).1;
    }
}

/// Whether `form` is carried in a projected row: only a value TOAST can
/// store out of the form `heap_form_tuple` gives it.
fn carried_form(value: &Datum, form: Option<&StoredForm>) -> Option<StoredForm> {
    form.copied()
        .filter(|form| *form != StoredForm::Inline)
        .filter(|_| crate::storage::rowenc::carries_stored_form(value))
}

/// Encodes state that must survive front rewinds used to retry resumable
/// execution. The statement reset still reclaims it as a unit.
pub(crate) fn encode_projected_persistent_pub<'a>(
    values: &[Datum],
    forms: &[StoredForm],
    arena: &'a Arena,
) -> Result<&'a [u8], SqlError> {
    let len = projected_row_len_formed(values, forms)?;
    let out = arena
        .alloc_persistent_slice_with(len, |_| 0u8)
        .map_err(|_| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "mutable routine arguments exceed statement memory"
            )
        })?;
    encode_projected_formed_into(values, forms, out)?;
    Ok(&*out)
}

/// Packs rows before transferring them out of a scoped body workspace.
pub(crate) fn pack_projected_rows<'a>(
    rows: &[&[u8]],
    arena: &'a Arena,
) -> Result<&'a [u8], SqlError> {
    let output = arena
        .alloc_slice_with(projected_rows_len(rows)?, |_| 0u8)
        .map_err(|_| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "routine rows exceed statement memory"
            )
        })?;
    pack_projected_rows_into(rows, output)?;
    Ok(output)
}

pub(crate) fn projected_rows_len(rows: &[&[u8]]) -> Result<usize, SqlError> {
    let full = || {
        sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "routine rows exceed statement memory"
        )
    };
    u64::try_from(rows.len()).map_err(|_| full())?;
    let mut len = 8_usize;
    for row in rows {
        len = len
            .checked_add(8)
            .and_then(|len| len.checked_add(row.len()))
            .ok_or_else(full)?;
    }
    Ok(len)
}

pub(crate) fn pack_projected_rows_into(rows: &[&[u8]], output: &mut [u8]) -> Result<(), SqlError> {
    let len = projected_rows_len(rows)?;
    if output.len() < len {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "routine rows exceed destination memory"
        ));
    }
    let count = u64::try_from(rows.len()).expect("projected_rows_len checked row count");
    output[..8].copy_from_slice(&count.to_le_bytes());
    let mut at = 8;
    for row in rows {
        let size = u64::try_from(row.len()).map_err(|_| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "routine row exceeds addressable size"
            )
        })?;
        output[at..at + 8].copy_from_slice(&size.to_le_bytes());
        at += 8;
        output[at..at + row.len()].copy_from_slice(row);
        at += row.len();
    }
    Ok(())
}

pub(crate) fn unpack_projected_rows<'a>(
    encoded: &'a [u8],
    arena: &'a Arena,
) -> Result<&'a [&'a [u8]], SqlError> {
    let invalid = || {
        sql_err!(
            sqlstate::INTERNAL_ERROR,
            "routine row transfer has an invalid encoding"
        )
    };
    let count = encoded.get(..8).ok_or_else(invalid)?;
    let count = usize::try_from(u64::from_le_bytes(
        count.try_into().expect("checked header"),
    ))
    .map_err(|_| invalid())?;
    if count > (encoded.len() - 8) / 8 {
        return Err(invalid());
    }
    let rows = arena.alloc_slice_with(count, |_| &[][..]).map_err(|_| {
        sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "routine rows exceed statement memory"
        )
    })?;
    let mut at = 8_usize;
    for row in rows.iter_mut() {
        let end = at.checked_add(8).ok_or_else(invalid)?;
        let size = encoded.get(at..end).ok_or_else(invalid)?;
        let size = usize::try_from(u64::from_le_bytes(size.try_into().expect("checked header")))
            .map_err(|_| invalid())?;
        let end = end.checked_add(size).ok_or_else(invalid)?;
        *row = encoded.get(at + 8..end).ok_or_else(invalid)?;
        at = end;
    }
    if at != encoded.len() {
        return Err(invalid());
    }
    Ok(rows)
}

/// Exact byte length of a projected row whose values are read by index.
pub(crate) fn projected_row_len_by<'a>(
    count: usize,
    mut value_at: impl FnMut(usize) -> Result<(Datum<'a>, StoredForm), SqlError>,
) -> Result<usize, SqlError> {
    let count_u16 = u16::try_from(count).map_err(|_| {
        sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "projected row has too many columns"
        )
    })?;
    let mut len = size_of_val(&count_u16);
    for index in 0..count {
        let (value, form) = value_at(index)?;
        let form = carried_form(&value, Some(&form)).map_or(0, |_| 1 + STORED_FORM_RECORD);
        len = len
            .checked_add(projected_value_len(&value)? + form)
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "projected row is too large"
                )
            })?;
    }
    Ok(len)
}

/// Writes an indexed projected row, each value with the stored form it
/// carries, into caller-owned external-run storage.
pub(crate) fn encode_projected_by_into<'a>(
    count: usize,
    mut value_at: impl FnMut(usize) -> Result<(Datum<'a>, StoredForm), SqlError>,
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
        let (value, form) = value_at(index)?;
        at += write_formed_value(&value, form, &mut out[at..]);
    }
    debug_assert_eq!(at, len);
    Ok(at)
}

/// Writes `value` with the form record it carries, if any.
fn write_formed_value(value: &Datum, form: StoredForm, out: &mut [u8]) -> usize {
    let mut at = 0;
    if let Some(form) = carried_form(value, Some(&form)) {
        out[0] = STORED_FORM_TAG;
        out[1..=STORED_FORM_RECORD].copy_from_slice(&form_record(form));
        at = 1 + STORED_FORM_RECORD;
    }
    at + write_projected_value(value, &mut out[at..])
}

/// Exact byte length needed by [`encode_projected_into`].
pub(crate) fn projected_row_len(values: &[Datum]) -> Result<usize, SqlError> {
    projected_row_len_formed(values, &[])
}

/// [`projected_row_len`] of values carrying `forms`.
pub(crate) fn projected_row_len_formed(
    values: &[Datum],
    forms: &[StoredForm],
) -> Result<usize, SqlError> {
    let count = u16::try_from(values.len()).map_err(|_| {
        sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "projected row has too many columns"
        )
    })?;
    let mut len = size_of_val(&count);
    for (index, v) in values.iter().enumerate() {
        let form = carried_form(v, forms.get(index)).map_or(0, |_| 1 + STORED_FORM_RECORD);
        len = len
            .checked_add(projected_value_len(v)? + form)
            .ok_or_else(|| {
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
    encode_projected_formed_into(values, &[], out)
}

/// [`encode_projected_into`] of values carrying `forms`.
pub(crate) fn encode_projected_formed_into(
    values: &[Datum],
    forms: &[StoredForm],
    out: &mut [u8],
) -> Result<usize, SqlError> {
    let len = projected_row_len_formed(values, forms)?;
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
    for (index, v) in values.iter().enumerate() {
        let form = forms.get(index).copied().unwrap_or(StoredForm::Inline);
        at += write_formed_value(v, form, &mut out[at..]);
    }
    debug_assert_eq!(at, len);
    Ok(at)
}

/// Number of values stored in a projected row.
pub(crate) fn projected_row_width(bytes: &[u8]) -> usize {
    u16::from_le_bytes(bytes[..2].try_into().expect("projected row header")) as usize
}

/// The projected-encoding byte length of one value (tag + payload).
pub fn projected_value_len(v: &Datum) -> Result<usize, SqlError> {
    Ok(1 + match v {
        Datum::Null | Datum::PgDdlCommand => 0,
        Datum::PgNodeTree(node) => 4 + node.as_str().len(),
        Datum::Bool(_) => 1,
        Datum::Char(_) => 1,
        Datum::Int2(_) => 2,
        Datum::Float4(_) => 4,
        Datum::Int4(_) | Datum::Oid(_) | Datum::Cid(_) | Datum::Date(_) => 4,
        Datum::Tid(_) => 6,
        Datum::Int8(_)
        | Datum::Xid8(_)
        | Datum::PgLsn(_)
        | Datum::Money(_)
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
        Datum::Text(s) | Datum::Bpchar(s) | Datum::Xml(s) => 4 + s.len(),
        Datum::AclItem(item) => 4 + item.raw().len(),
        Datum::TsVector(text) => 4 + text.len(),
        Datum::TsQuery(text) => 4 + text.len(),
        Datum::Regtype { name, .. } => 8 + name.len(),
        Datum::RegObject { name, .. } => 12 + name.len(),
        Datum::Json { text, .. } => 5 + text.len(),
        Datum::JsonPath(text) => 4 + text.len(),
        Datum::Snapshot { value, .. } => 5 + value.raw().len(),
        // Compact element descriptor plus byte length.
        Datum::Array { raw, .. } => crate::storage::CACHED_ARRAY_ELEMENT_BYTES + 4 + raw.len(),
        Datum::Int2Vector { raw, .. } | Datum::OidVector { raw, .. } => 8 + raw.len(),
        Datum::Bytea(b) => 4 + b.len(),
        Datum::Numeric(nm) => 7 + nm.digits.len(),
        Datum::Range { text, .. } => 5 + text.len(),
        Datum::Bit { bits, .. } => 5 + bits.len(),
        Datum::Multirange { text, .. } => 5 + text.len(),
        Datum::Geometry { text, .. } => 5 + text.len(),
        // Schema-less enums retain creation(4), sort(8), member(4) and label length(4).
        Datum::Enum { label, .. } => 20 + label.len(),
        // A record stores its rendered text (the arena-free decode returns
        // that, keeping comparators and output unchanged) followed by a
        // structural tail — field names, OIDs, nested tagged values — that
        // [`decode_projected_col_record`] rebuilds into a `Datum::Record`
        // when a consumer needs field access.
        Datum::Record(fields) | Datum::Composite { fields, .. } => {
            let length_bytes = RecordLengths::for_fields(fields)?.bytes();
            let mut n = 4 + record_text_len(v) + length_bytes;
            if matches!(v, Datum::Composite { .. }) {
                n += 4;
            }
            for f in *fields {
                n += length_bytes + f.name.len() + 8 + projected_value_len(&f.value)?;
                if carried_form(&f.value, Some(&f.form)).is_some() {
                    n += 1 + STORED_FORM_RECORD;
                }
            }
            n
        }
        Datum::CompositeText {
            physical_fields,
            text,
            ..
        } => 4 + physical_field_count_len(*physical_fields) + 4 + text.raw().len(),
    })
}

#[derive(Clone, Copy)]
enum RecordLengths {
    Byte,
    TwoBytes,
}

impl RecordLengths {
    fn for_fields(fields: &[crate::sql::types::RecordField<'_>]) -> Result<Self, SqlError> {
        if fields.len() > u16::MAX as usize
            || fields
                .iter()
                .any(|field| field.name.len() > u16::MAX as usize)
        {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "record field count or name exceeds projected encoding capacity"
            ));
        }
        Ok(
            if fields.len() > u8::MAX as usize
                || fields
                    .iter()
                    .any(|field| field.name.len() > u8::MAX as usize)
            {
                Self::TwoBytes
            } else {
                Self::Byte
            },
        )
    }

    const fn bytes(self) -> usize {
        match self {
            Self::Byte => 1,
            Self::TwoBytes => 2,
        }
    }

    fn write(self, value: usize, out: &mut [u8]) {
        match self {
            Self::Byte => {
                out[0] =
                    u8::try_from(value).expect("projected_value_len checked narrow record lengths")
            }
            Self::TwoBytes => out[..2].copy_from_slice(
                &u16::try_from(value)
                    .expect("projected_value_len checked wide record lengths")
                    .to_le_bytes(),
            ),
        }
    }

    fn read(self, bytes: &[u8], at: usize) -> usize {
        match self {
            Self::Byte => bytes[at] as usize,
            Self::TwoBytes => u16::from_le_bytes(bytes[at..at + 2].try_into().unwrap()) as usize,
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
        Datum::PgDdlCommand => {
            out[0] = 36;
            1
        }
        Datum::PgNodeTree(node) => {
            let encoded = node.as_str().as_bytes();
            out[0] = 50;
            out[1..5].copy_from_slice(&(encoded.len() as u32).to_le_bytes());
            out[5..5 + encoded.len()].copy_from_slice(encoded);
            5 + encoded.len()
        }
        Datum::Bool(b) => {
            out[0] = 1;
            out[1] = u8::from(*b);
            2
        }
        Datum::Char(byte) => {
            out[0] = 40;
            out[1] = *byte;
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
        Datum::Xid8(x) => {
            out[0] = 44;
            out[1..9].copy_from_slice(&x.to_le_bytes());
            9
        }
        Datum::Snapshot { value, legacy } => {
            out[0] = 45;
            out[1] = u8::from(*legacy);
            out[2..6].copy_from_slice(&(value.raw().len() as u32).to_le_bytes());
            out[6..6 + value.raw().len()].copy_from_slice(value.raw());
            6 + value.raw().len()
        }
        Datum::PgLsn(x) => {
            out[0] = 41;
            out[1..9].copy_from_slice(&x.to_le_bytes());
            9
        }
        Datum::Money(x) => {
            out[0] = 46;
            out[1..9].copy_from_slice(&x.to_le_bytes());
            9
        }
        Datum::Tid(x) => {
            out[0] = 47;
            out[1..5].copy_from_slice(&x.block.to_le_bytes());
            out[5..7].copy_from_slice(&x.offset.to_le_bytes());
            7
        }
        Datum::Cid(x) => {
            out[0] = 48;
            out[1..5].copy_from_slice(&x.to_le_bytes());
            5
        }
        Datum::AclItem(item) => {
            out[0] = 49;
            out[1..5].copy_from_slice(&(item.raw().len() as u32).to_le_bytes());
            out[5..5 + item.raw().len()].copy_from_slice(item.raw());
            5 + item.raw().len()
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
            out[0] = PROJECTED_TEXT_TAG;
            out[1..5].copy_from_slice(&(str_value.len() as u32).to_le_bytes());
            out[5..5 + str_value.len()].copy_from_slice(str_value.as_bytes());
            5 + str_value.len()
        }
        Datum::TsVector(text) => {
            out[0] = 37;
            out[1..5].copy_from_slice(&(text.len() as u32).to_le_bytes());
            out[5..5 + text.len()].copy_from_slice(text.as_bytes());
            5 + text.len()
        }
        Datum::TsQuery(text) => {
            out[0] = 38;
            out[1..5].copy_from_slice(&(text.len() as u32).to_le_bytes());
            out[5..5 + text.len()].copy_from_slice(text.as_bytes());
            5 + text.len()
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
        Datum::JsonPath(text) => {
            out[0] = 42;
            out[1..5].copy_from_slice(&(text.len() as u32).to_le_bytes());
            out[5..5 + text.len()].copy_from_slice(text.as_bytes());
            5 + text.len()
        }
        Datum::Xml(text) => {
            out[0] = 43;
            out[1..5].copy_from_slice(&(text.len() as u32).to_le_bytes());
            out[5..5 + text.len()].copy_from_slice(text.as_bytes());
            5 + text.len()
        }
        Datum::Array { element, raw } => {
            // A nested element (a statistics array's member arrays) is
            // described by its members' element.
            let (tag, described) = match element.nested_member() {
                Some(member) => (205, member),
                None => (200, *element),
            };
            out[0] = tag;
            let descriptor = crate::storage::CachedArrayElement::from_element(described)
                .expect("projected array type was bound");
            const D: usize = crate::storage::CACHED_ARRAY_ELEMENT_BYTES;
            out[1..1 + D].copy_from_slice(&descriptor.bytes());
            out[1 + D..5 + D].copy_from_slice(&(raw.len() as u32).to_le_bytes());
            out[5 + D..5 + D + raw.len()].copy_from_slice(raw);
            5 + D + raw.len()
        }
        Datum::Int2Vector { raw, lower_bound } => {
            out[0] = 29;
            out[1..5].copy_from_slice(&(raw.len() as u32).to_le_bytes());
            out[5..9].copy_from_slice(&lower_bound.to_le_bytes());
            out[9..9 + raw.len()].copy_from_slice(raw);
            9 + raw.len()
        }
        Datum::OidVector { raw, lower_bound } => {
            out[0] = 34;
            out[1..5].copy_from_slice(&(raw.len() as u32).to_le_bytes());
            out[5..9].copy_from_slice(&lower_bound.to_le_bytes());
            out[9..9 + raw.len()].copy_from_slice(raw);
            9 + raw.len()
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
                crate::sql::numeric::Sign::PosInf => 3,
                crate::sql::numeric::Sign::NegInf => 4,
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
        Datum::Geometry { kind, text } => {
            out[0] = 39;
            out[1] = *kind as u8;
            out[2..6].copy_from_slice(&(text.len() as u32).to_le_bytes());
            out[6..6 + text.len()].copy_from_slice(text.as_bytes());
            6 + text.len()
        }
        Datum::Enum {
            slot,
            member,
            sort,
            label,
        } => {
            out[0] = 201;
            out[1..5].copy_from_slice(&slot.to_le_bytes());
            out[5..13].copy_from_slice(&sort.to_le_bytes());
            out[13..17].copy_from_slice(
                &member
                    .map_or(u32::MAX, crate::sql::types::EnumMemberId::ordinal)
                    .to_le_bytes(),
            );
            out[17..21].copy_from_slice(&(label.len() as u32).to_le_bytes());
            out[21..21 + label.len()].copy_from_slice(label.as_bytes());
            21 + label.len()
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
            let lengths = RecordLengths::for_fields(fields)
                .expect("projected_value_len checked record lengths");
            let length_bytes = lengths.bytes();
            out[0] = if length_bytes == 1 { 19 } else { 51 };
            let mut w = SliceWriter { buf: out, at: 5 };
            let _ = write!(w, "{v}");
            let text_len = w.at - 5;
            out[1..5].copy_from_slice(&(text_len as u32).to_le_bytes());
            // Structural tail: field count, then per field its name, type
            // OID and modifier, and nested tagged value.
            let mut at = 5 + text_len;
            lengths.write(fields.len(), &mut out[at..]);
            at += length_bytes;
            for f in *fields {
                lengths.write(f.name.len(), &mut out[at..]);
                at += length_bytes;
                out[at..at + f.name.len()].copy_from_slice(f.name.as_bytes());
                at += f.name.len();
                out[at..at + 4].copy_from_slice(&f.type_oid.to_le_bytes());
                out[at + 4..at + 8].copy_from_slice(&f.type_mod.to_le_bytes());
                at += 8;
                at += write_formed_value(&f.value, f.form, &mut out[at..]);
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
            let lengths = RecordLengths::for_fields(fields)
                .expect("projected_value_len checked record lengths");
            let length_bytes = lengths.bytes();
            out[0] = if length_bytes == 1 { 202 } else { 204 };
            out[1..5].copy_from_slice(&slot.to_le_bytes());
            let mut w = SliceWriter { buf: out, at: 9 };
            let _ = write!(w, "{v}");
            let text_len = w.at - 9;
            out[5..9].copy_from_slice(&(text_len as u32).to_le_bytes());
            let mut at = 9 + text_len;
            lengths.write(fields.len(), &mut out[at..]);
            at += length_bytes;
            for f in *fields {
                lengths.write(f.name.len(), &mut out[at..]);
                at += length_bytes;
                out[at..at + f.name.len()].copy_from_slice(f.name.as_bytes());
                at += f.name.len();
                out[at..at + 4].copy_from_slice(&f.type_oid.to_le_bytes());
                out[at + 4..at + 8].copy_from_slice(&f.type_mod.to_le_bytes());
                at += 8;
                at += write_formed_value(&f.value, f.form, &mut out[at..]);
            }
            at
        }
        Datum::CompositeText {
            slot,
            physical_fields,
            text,
        } => {
            let text = text.raw();
            out[0] = 203;
            out[1..5].copy_from_slice(&slot.to_le_bytes());
            let prefix = encode_physical_field_count(*physical_fields, &mut out[5..]);
            out[5 + prefix..9 + prefix].copy_from_slice(&(text.len() as u32).to_le_bytes());
            out[9 + prefix..9 + prefix + text.len()].copy_from_slice(text.as_bytes());
            9 + prefix + text.len()
        }
    }
}

/// Reads the value whose tag is `tag` at byte `at`, returning it and its
/// payload length. This is the one place the projected encoding's tag
/// sizes live: a second, hand-written copy in the sort path drifted from
/// it and panicked the server on every tag it had not been taught.
/// The tag of an encoded `text` value: a little-endian `u32` length, then
/// its UTF-8 bytes.
pub(crate) const PROJECTED_TEXT_TAG: u8 = 5;

/// The encoded length of the value tagged `tag` at `at`, the size
/// [`decode_projected_value`] returns, read without decoding fixed-width
/// values or text.
pub(crate) fn projected_value_size(bytes: &[u8], tag: u8, at: usize) -> usize {
    let size = match tag {
        0 | 36 => 0,
        1 | 40 => 1,
        2 | 35 | 48 => 4,
        47 => 6,
        41 | 44 | 46 => 8,
        PROJECTED_TEXT_TAG => {
            4 + u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize
        }
        _ => return decode_projected_value(bytes, tag, at).1,
    };
    debug_assert_eq!(size, decode_projected_value(bytes, tag, at).1);
    size
}

pub fn decode_projected_value(bytes: &[u8], tag: u8, at: usize) -> (Datum<'_>, usize) {
    match tag {
        0 => (Datum::Null, 0),
        STORED_FORM_TAG => {
            let inner = at + STORED_FORM_RECORD;
            let (value, size) = decode_projected_value(bytes, bytes[inner], inner + 1);
            (value, STORED_FORM_RECORD + 1 + size)
        }
        36 => (Datum::PgDdlCommand, 0),
        50 => {
            let len = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
            let encoded = core::str::from_utf8(&bytes[at + 4..at + 4 + len])
                .expect("projected catalog node was encoded from valid UTF-8");
            let node = crate::sql::catalog_node::CatalogNode::from_encoded(encoded)
                .expect("projected catalog node was validated before decoding");
            (Datum::PgNodeTree(node), 4 + len)
        }
        1 => (Datum::Bool(bytes[at] != 0), 1),
        40 => (Datum::Char(bytes[at]), 1),
        2 => (
            Datum::Int4(i32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())),
            4,
        ),
        35 => (
            Datum::Oid(u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())),
            4,
        ),
        44 => (
            Datum::Xid8(u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())),
            8,
        ),
        45 => {
            let legacy = bytes[at] != 0;
            let len = u32::from_le_bytes(bytes[at + 1..at + 5].try_into().unwrap()) as usize;
            (
                Datum::Snapshot {
                    value: crate::sql::snapshot::Snapshot::restore(&bytes[at + 5..at + 5 + len])
                        .expect("projected snapshot was validated before encoding"),
                    legacy,
                },
                5 + len,
            )
        }
        41 => (
            Datum::PgLsn(u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())),
            8,
        ),
        46 => (
            Datum::Money(i64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())),
            8,
        ),
        47 => (
            Datum::Tid(crate::sql::types::Tid {
                block: u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()),
                offset: u16::from_le_bytes(bytes[at + 4..at + 6].try_into().unwrap()),
            }),
            6,
        ),
        48 => (
            Datum::Cid(u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())),
            4,
        ),
        49 => {
            let len = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
            (
                Datum::AclItem(
                    crate::sql::acl::from_stored(&bytes[at + 4..at + 4 + len])
                        .expect("encoded from a valid aclitem"),
                ),
                4 + len,
            )
        }
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
        PROJECTED_TEXT_TAG => {
            let len = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
            (
                Datum::Text(
                    core::str::from_utf8(&bytes[at + 4..at + 4 + len])
                        .expect("encoded from valid UTF-8"),
                ),
                4 + len,
            )
        }
        37 | 38 => {
            let len = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
            let text = core::str::from_utf8(&bytes[at + 4..at + 4 + len])
                .expect("projected text-search value was encoded from valid UTF-8");
            (
                if tag == 37 {
                    Datum::TsVector(crate::sql::full_text::restore_vector(text))
                } else {
                    Datum::TsQuery(crate::sql::full_text::restore_query(text))
                },
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
        42 => {
            let len = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
            let text = core::str::from_utf8(&bytes[at + 4..at + 4 + len])
                .expect("projected jsonpath was encoded from valid UTF-8");
            (Datum::JsonPath(text), 4 + len)
        }
        43 => {
            let len = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
            (
                Datum::Xml(core::str::from_utf8(&bytes[at + 4..at + 4 + len]).unwrap()),
                4 + len,
            )
        }
        tag @ (200 | 205) => {
            const D: usize = crate::storage::CACHED_ARRAY_ELEMENT_BYTES;
            let described = crate::storage::CachedArrayElement::from_bytes(
                bytes[at..at + D].try_into().unwrap(),
            )
            .expect("projected array descriptor was checked")
            .element();
            let element = if tag == 205 {
                crate::sql::types::ArrElem::nested(described)
                    .expect("projected nested array describes its member element")
            } else {
                described
            };
            let len = u32::from_le_bytes(bytes[at + D..at + D + 4].try_into().unwrap()) as usize;
            (
                Datum::Array {
                    element,
                    raw: &bytes[at + D + 4..at + D + 4 + len],
                },
                D + 4 + len,
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
        39 => {
            let kind = crate::sql::types::GeometryKind::ALL
                .into_iter()
                .find(|kind| *kind as u8 == bytes[at])
                .expect("projected geometry carries a valid kind code");
            let len = u32::from_le_bytes(bytes[at + 1..at + 5].try_into().unwrap()) as usize;
            let text = core::str::from_utf8(&bytes[at + 5..at + 5 + len])
                .expect("projected geometry was encoded from valid UTF-8");
            (Datum::Geometry { kind, text }, 5 + len)
        }
        19 | 51 => {
            // The arena-free decode returns a record's rendered text — right
            // for comparators and output. Field access goes through
            // [`decode_projected_col_record`], which rebuilds the structure
            // from the tail this arm skips over.
            let len = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
            let s = core::str::from_utf8(&bytes[at + 4..at + 4 + len])
                .expect("projected record text was encoded from valid UTF-8");
            (
                Datum::Text(s),
                4 + len
                    + record_tail_len(
                        bytes,
                        at + 4 + len,
                        if tag == 19 {
                            RecordLengths::Byte
                        } else {
                            RecordLengths::TwoBytes
                        },
                    ),
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
                3 => crate::sql::numeric::Sign::PosInf,
                4 => crate::sql::numeric::Sign::NegInf,
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
        201 => {
            let slot =
                crate::storage::TypeCreation::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
                    .expect("projected enum creation");
            let sort = f64::from_le_bytes(bytes[at + 4..at + 12].try_into().unwrap());
            let identity = u32::from_le_bytes(bytes[at + 12..at + 16].try_into().unwrap());
            let member = crate::sql::types::EnumMemberId::from_ordinal(identity);
            let len = u32::from_le_bytes(bytes[at + 16..at + 20].try_into().unwrap()) as usize;
            let label = core::str::from_utf8(&bytes[at + 20..at + 20 + len])
                .expect("projected enum label is valid UTF-8");
            (
                Datum::Enum {
                    slot,
                    member,
                    sort,
                    label,
                },
                20 + len,
            )
        }
        29 => {
            let len = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
            (
                Datum::Int2Vector {
                    lower_bound: i32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap()),
                    raw: &bytes[at + 8..at + 8 + len],
                },
                8 + len,
            )
        }
        34 => {
            let len = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
            (
                Datum::OidVector {
                    lower_bound: i32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap()),
                    raw: &bytes[at + 8..at + 8 + len],
                },
                8 + len,
            )
        }
        202 | 204 => {
            let len = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap()) as usize;
            let s = core::str::from_utf8(&bytes[at + 8..at + 8 + len])
                .expect("projected composite text was encoded from valid UTF-8");
            (
                Datum::Text(s),
                8 + len
                    + record_tail_len(
                        bytes,
                        at + 8 + len,
                        if tag == 202 {
                            RecordLengths::Byte
                        } else {
                            RecordLengths::TwoBytes
                        },
                    ),
            )
        }
        203 => {
            let slot =
                crate::storage::TypeCreation::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
                    .expect("projected composite creation");
            let (physical_fields, prefix) =
                decode_physical_field_count(&bytes[at + 4..]).expect("projected composite prefix");
            let length_at = at + 4 + prefix;
            let len =
                u32::from_le_bytes(bytes[length_at..length_at + 4].try_into().unwrap()) as usize;
            let text = crate::sql::types::CompositeSpelling::parse(
                core::str::from_utf8(&bytes[length_at + 4..length_at + 4 + len])
                    .expect("projected composite text was encoded from valid UTF-8"),
            )
            .expect("projected composite text was encoded from a spelling");
            (
                Datum::CompositeText {
                    slot,
                    physical_fields,
                    text,
                },
                4 + prefix + 4 + len,
            )
        }
        _ => unreachable!("tags are exhaustive"),
    }
}

const fn physical_field_count_len(fields: u16) -> usize {
    if fields < u8::MAX as u16 { 1 } else { 3 }
}

fn encode_physical_field_count(fields: u16, output: &mut [u8]) -> usize {
    if fields < u8::MAX as u16 {
        output[0] = fields as u8;
        1
    } else {
        output[0] = u8::MAX;
        output[1..3].copy_from_slice(&fields.to_le_bytes());
        3
    }
}

fn decode_physical_field_count(input: &[u8]) -> Option<(u16, usize)> {
    let first = *input.first()?;
    if first != u8::MAX {
        Some((u16::from(first), 1))
    } else {
        Some((u16::from_le_bytes(input.get(1..3)?.try_into().ok()?), 3))
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
/// Compares two encoded rows' first `width` columns under SQL equality, NULLs
/// equal to each other and last: DISTINCT must merge equal values spelled
/// differently (`1.0` and `1.00`, `'a '::char(3)` and `'a'::char(1)`) and
/// record values by `record_cmp`, stored named composites by their catalog
/// fields as of `txid`. Text compares bytewise, which agrees with equality
/// under every deterministic collation; callers needing collation order
/// compare keys with their collation first.
pub(crate) fn compare_projected_prefix(
    storage: &crate::storage::Storage,
    txid: u32,
    a: &[u8],
    b: &[u8],
    width: usize,
) -> Result<core::cmp::Ordering, SqlError> {
    let (mut ia, mut ib) = (2usize, 2usize);
    for _ in 0..width {
        let left = ProjectedValue::at(a, ia);
        let right = ProjectedValue::at(b, ib);
        let ord = match (left.is_null(), right.is_null()) {
            (true, true) => core::cmp::Ordering::Equal,
            (true, false) => core::cmp::Ordering::Greater,
            (false, true) => core::cmp::Ordering::Less,
            (false, false) => compare_projected_values(left, right, &mut |x, y| {
                crate::sql::eval::compare_datums_collated(
                    storage,
                    txid,
                    crate::sql::ast::Collation::C,
                    x,
                    y,
                )
            })?,
        };
        if !ord.is_eq() {
            return Ok(ord);
        }
        ia += 1 + decode_projected_value(a, a[ia], ia + 1).1;
        ib += 1 + decode_projected_value(b, b[ib], ib + 1).1;
    }
    Ok(core::cmp::Ordering::Equal)
}

/// Orders two whole encoded rows under SQL equality as
/// [`compare_projected_prefix`] does; the set operations and recursive
/// UNION sort, merge and deduplicate rows by this order.
pub(crate) fn compare_projected_rows(
    storage: &crate::storage::Storage,
    txid: u32,
    a: &[u8],
    b: &[u8],
) -> Result<core::cmp::Ordering, SqlError> {
    let width = projected_row_width(a);
    match width.cmp(&projected_row_width(b)) {
        core::cmp::Ordering::Equal => compare_projected_prefix(storage, txid, a, b, width),
        different => Ok(different),
    }
}

/// One tagged value inside an encoded row or record, located by its tag
/// byte. Comparators take this instead of a decoded [`Datum`] because the
/// arena-free decode renders a record as text, which orders `(10)` before
/// `(9)`; the structural tail kept after that text is what `record_cmp`
/// needs.
#[derive(Clone, Copy)]
pub(crate) struct ProjectedValue<'a> {
    bytes: &'a [u8],
    tag_at: usize,
}

impl<'a> ProjectedValue<'a> {
    /// Column `col` of an encoded row.
    pub(crate) fn column(bytes: &'a [u8], col: usize) -> Self {
        Self::at(bytes, Self::column_tag_at(bytes, col))
    }

    /// The tag position of column `col`, before any form record.
    fn column_tag_at(bytes: &[u8], col: usize) -> usize {
        let mut at = 2usize;
        for _ in 0..col {
            let tag = bytes[at];
            at += 1 + projected_value_size(bytes, tag, at + 1);
        }
        at
    }

    /// The value whose tag, or form record, is at `tag_at` (such as a
    /// [`Self::offset`]): the value's own tag, past the form record it may
    /// carry.
    pub(crate) fn at(bytes: &'a [u8], tag_at: usize) -> Self {
        let tag_at = if bytes[tag_at] == STORED_FORM_TAG {
            tag_at + 1 + STORED_FORM_RECORD
        } else {
            tag_at
        };
        Self { bytes, tag_at }
    }

    /// Where this value starts in its row, for [`Self::at`].
    pub(crate) fn offset(self) -> usize {
        self.tag_at
    }

    /// The column after this one, which the row must have.
    pub(crate) fn next(self) -> Self {
        let tag = self.bytes[self.tag_at];
        Self::at(
            self.bytes,
            self.tag_at + 1 + projected_value_size(self.bytes, tag, self.tag_at + 1),
        )
    }

    /// The value as the arena-free decode returns it.
    pub(crate) fn datum(self) -> Datum<'a> {
        decode_projected_value(self.bytes, self.bytes[self.tag_at], self.tag_at + 1).0
    }

    pub(crate) fn is_null(self) -> bool {
        self.bytes[self.tag_at] == 0
    }

    pub(crate) fn is_record(self) -> bool {
        self.record_tail().is_some()
    }

    /// A record value's fields in order, or `None` when this is not a record.
    pub(crate) fn record_fields(self) -> Option<ProjectedFields<'a>> {
        let (at, lengths) = self.record_tail()?;
        Some(ProjectedFields {
            bytes: self.bytes,
            remaining: lengths.read(self.bytes, at),
            at: at + lengths.bytes(),
            lengths,
        })
    }

    /// The structural tail of a record value: its field-count position and
    /// length width.
    fn record_tail(self) -> Option<(usize, RecordLengths)> {
        let at = self.tag_at + 1;
        let text_len = |from: usize| {
            u32::from_le_bytes(self.bytes[from..from + 4].try_into().unwrap()) as usize
        };
        match self.bytes[self.tag_at] {
            19 => Some((at + 4 + text_len(at), RecordLengths::Byte)),
            51 => Some((at + 4 + text_len(at), RecordLengths::TwoBytes)),
            202 => Some((at + 8 + text_len(at + 4), RecordLengths::Byte)),
            204 => Some((at + 8 + text_len(at + 4), RecordLengths::TwoBytes)),
            _ => None,
        }
    }
}

/// The fields of an encoded record: each field's name, type OID and value.
pub(crate) struct ProjectedFields<'a> {
    bytes: &'a [u8],
    remaining: usize,
    at: usize,
    lengths: RecordLengths,
}

impl<'a> Iterator for ProjectedFields<'a> {
    type Item = (&'a str, i32, ProjectedValue<'a>);

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        self.remaining -= 1;
        let name_len = self.lengths.read(self.bytes, self.at);
        let name_at = self.at + self.lengths.bytes();
        let name = core::str::from_utf8(&self.bytes[name_at..name_at + name_len])
            .expect("projected record name was encoded from valid UTF-8");
        let type_at = name_at + name_len;
        let type_oid = i32::from_le_bytes(self.bytes[type_at..type_at + 4].try_into().unwrap());
        let value = ProjectedValue::at(self.bytes, type_at + 8);
        let tag = self.bytes[type_at + 8];
        self.at = type_at + 9 + decode_projected_value(self.bytes, tag, type_at + 9).1;
        Some((name, type_oid, value))
    }
}

/// Orders two encoded sort-key values under an `ORDER BY` direction
/// `(descending, nulls_first)`: NULL placement is absolute, only the value
/// comparison reverses for DESC. Non-record values compare with `compare`
/// under `collation`; record values compare with `record_cmp`, their
/// collatable fields under the default collation (the collation a row type's
/// attributes carry).
pub(crate) fn compare_projected_key(
    collation: crate::sql::ast::Collation,
    (descending, nulls_first): (bool, bool),
    left: ProjectedValue<'_>,
    right: ProjectedValue<'_>,
    compare: &dyn Fn(
        crate::sql::ast::Collation,
        &Datum<'_>,
        &Datum<'_>,
    ) -> Result<core::cmp::Ordering, SqlError>,
) -> Result<core::cmp::Ordering, SqlError> {
    use core::cmp::Ordering;
    Ok(match (left.is_null(), right.is_null()) {
        (true, true) => Ordering::Equal,
        (true, false) if nulls_first => Ordering::Less,
        (true, false) => Ordering::Greater,
        (false, true) if nulls_first => Ordering::Greater,
        (false, true) => Ordering::Less,
        (false, false) => {
            let collation = if left.is_record() && right.is_record() {
                crate::sql::ast::Collation::Default
            } else {
                collation
            };
            let compared =
                compare_projected_values(left, right, &mut |x, y| compare(collation, x, y))?;
            if descending {
                compared.reverse()
            } else {
                compared
            }
        }
    })
}

/// Orders two non-NULL encoded values. Record values (named composites
/// included) follow PostgreSQL's `record_cmp`: fields compare left to right
/// with `leaf`, two NULL fields are equal and a NULL field sorts after any
/// value, nested records recurse, and dissimilar field types or widths are
/// errors. Every other value is decoded and handed to `leaf`.
pub(crate) fn compare_projected_values(
    left: ProjectedValue<'_>,
    right: ProjectedValue<'_>,
    leaf: &mut dyn FnMut(&Datum<'_>, &Datum<'_>) -> Result<core::cmp::Ordering, SqlError>,
) -> Result<core::cmp::Ordering, SqlError> {
    let (Some(mut a), Some(mut b)) = (left.record_fields(), right.record_fields()) else {
        return leaf(&left.datum(), &right.datum());
    };
    let mut column = 0usize;
    loop {
        let ((_, a_type, x), (_, b_type, y)) = match (a.next(), b.next()) {
            (Some(x), Some(y)) => (x, y),
            (None, None) => return Ok(core::cmp::Ordering::Equal),
            _ => {
                return Err(sql_err!(
                    sqlstate::DATATYPE_MISMATCH,
                    "cannot compare record types with different numbers of columns"
                ));
            }
        };
        crate::sql::eval::check_record_field_types(
            a_type,
            b_type,
            column,
            false,
            &crate::sql::eval::builtin_record_field_type_name,
        )?;
        let ordering = match (x.is_null(), y.is_null()) {
            (true, true) => core::cmp::Ordering::Equal,
            (true, false) => core::cmp::Ordering::Greater,
            (false, true) => core::cmp::Ordering::Less,
            (false, false) => compare_projected_values(x, y, leaf)?,
        };
        if !ordering.is_eq() {
            return Ok(ordering);
        }
        column += 1;
    }
}

/// The byte length of a record's structural tail starting at `at` (the field
/// count prefix), nested records included.
fn record_tail_len(bytes: &[u8], at: usize, lengths: RecordLengths) -> usize {
    let mut cursor = at;
    let length_bytes = lengths.bytes();
    let n_fields = lengths.read(bytes, cursor);
    cursor += length_bytes;
    for _ in 0..n_fields {
        let name_len = lengths.read(bytes, cursor);
        cursor += length_bytes + name_len + 8;
        let tag = bytes[cursor];
        cursor += 1;
        cursor += decode_projected_value(bytes, tag, cursor).1;
    }
    cursor - at
}

/// Calls `visit` with each field of the record value whose tag is `tag` at
/// byte `at` (its type OID, tag and payload offset), without allocating;
/// `None` when `tag` is not a record's or `visit` returns `None`.
pub(crate) fn try_each_record_field(
    bytes: &[u8],
    tag: u8,
    at: usize,
    visit: &mut dyn FnMut(i32, u8, usize) -> Option<()>,
) -> Option<()> {
    let (tail, lengths) = match tag {
        19 | 51 => {
            let text = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
            (
                at + 4 + text,
                if tag == 19 {
                    RecordLengths::Byte
                } else {
                    RecordLengths::TwoBytes
                },
            )
        }
        202 | 204 => {
            let text = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap()) as usize;
            (
                at + 8 + text,
                if tag == 202 {
                    RecordLengths::Byte
                } else {
                    RecordLengths::TwoBytes
                },
            )
        }
        _ => return None,
    };
    let length_bytes = lengths.bytes();
    let mut cursor = tail;
    let fields = lengths.read(bytes, cursor);
    cursor += length_bytes;
    for _ in 0..fields {
        let name = lengths.read(bytes, cursor);
        cursor += length_bytes + name;
        let type_oid = i32::from_le_bytes(bytes[cursor..cursor + 4].try_into().unwrap());
        cursor += 8;
        let field_tag = bytes[cursor];
        cursor += 1;
        visit(type_oid, field_tag, cursor)?;
        cursor += decode_projected_value(bytes, field_tag, cursor).1;
    }
    Some(())
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
            return restore_projected_record(bytes, tag, at, value, arena);
        }
        at += size;
        current += 1;
    }
}

/// Decodes a whole typed row in one pass, including nested record values.
pub(crate) fn decode_projected_row_into<'a>(
    bytes: &'a [u8],
    arena: &'a Arena,
    output: &mut [Datum<'a>],
) -> Result<usize, SqlError> {
    let width = projected_row_width(bytes);
    if width > output.len() {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "projected row has more than {} columns",
            output.len()
        ));
    }
    let mut at = 2usize;
    for value in &mut output[..width] {
        let tag = bytes[at];
        at += 1;
        let (decoded, size) = decode_projected_value(bytes, tag, at);
        *value = restore_projected_record(bytes, tag, at, decoded, arena)?;
        at += size;
    }
    Ok(width)
}

fn restore_projected_record<'a>(
    bytes: &'a [u8],
    tag: u8,
    at: usize,
    scalar: Datum<'a>,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    if matches!(tag, 19 | 51) {
        let text_len = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
        return decode_record_tail(
            bytes,
            at + 4 + text_len,
            if tag == 19 {
                RecordLengths::Byte
            } else {
                RecordLengths::TwoBytes
            },
            arena,
        );
    }
    if matches!(tag, 202 | 204) {
        let slot =
            crate::storage::TypeCreation::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
                .expect("projected composite creation");
        let text_len = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap()) as usize;
        let Datum::Record(fields) = decode_record_tail(
            bytes,
            at + 8 + text_len,
            if tag == 202 {
                RecordLengths::Byte
            } else {
                RecordLengths::TwoBytes
            },
            arena,
        )?
        else {
            unreachable!()
        };
        return Ok(Datum::Composite { slot, fields });
    }
    Ok(scalar)
}

/// Rebuilds a `Datum::Record` from the structural tail at `at`.
fn decode_record_tail<'a>(
    bytes: &'a [u8],
    at: usize,
    lengths: RecordLengths,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    use crate::sql::types::RecordField;
    let length_bytes = lengths.bytes();
    let mut cursor = at;
    let n_fields = lengths.read(bytes, cursor);
    cursor += length_bytes;
    let fields = arena
        .alloc_slice_with(n_fields, |_| RecordField {
            form: crate::storage::StoredForm::Inline,
            name: "",
            type_oid: 0,
            type_mod: -1,
            value: Datum::Null,
        })
        .map_err(|_| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "record decode exceeds the statement arena"
            )
        })?;
    for f in fields.iter_mut() {
        let name_len = lengths.read(bytes, cursor);
        cursor += length_bytes;
        f.name = core::str::from_utf8(&bytes[cursor..cursor + name_len])
            .expect("projected record name was encoded from valid UTF-8");
        cursor += name_len;
        f.type_oid = i32::from_le_bytes(bytes[cursor..cursor + 4].try_into().unwrap());
        f.type_mod = i32::from_le_bytes(bytes[cursor + 4..cursor + 8].try_into().unwrap());
        cursor += 8;
        f.form = projected_stored_form_at(bytes, cursor);
        if bytes[cursor] == STORED_FORM_TAG {
            cursor += 1 + STORED_FORM_RECORD;
        }
        let tag = bytes[cursor];
        cursor += 1;
        let (value, size) = decode_projected_value(bytes, tag, cursor);
        f.value = restore_projected_record(bytes, tag, cursor, value, arena)?;
        cursor += size;
    }
    Ok(Datum::Record(fields))
}

/// DISTINCT over encoded rows: sorts (grouping SQL-equal rows adjacently,
/// byte order as the tiebreak so the surviving representative is
/// deterministic) and keeps the first of each run. Returns the live count.
pub fn sort_dedup_projected(
    storage: &crate::storage::Storage,
    txid: u32,
    rows: &mut [&[u8]],
    width: usize,
) -> Result<usize, SqlError> {
    crate::util::try_sort_unstable_by(rows, |a, b| {
        Ok(compare_projected_prefix(storage, txid, a, b, width)?.then_with(|| a.cmp(b)))
    })?;
    let mut unique = 0usize;
    for i in 0..rows.len() {
        let same = i > 0
            && compare_projected_prefix(storage, txid, rows[i], rows[unique - 1], width)?.is_eq();
        if !same {
            rows[unique] = rows[i];
            unique += 1;
        }
    }
    Ok(unique)
}

#[cfg(all(
    test,
    not(all(miri, feature = "miri-ownership")),
    not(feature = "sql-fixture-suite")
))]
mod tests {
    use super::*;
    use crate::mem::budget::Budget;

    #[test]
    fn row_sets_transfer_empty_and_typed_rows_without_child_references() {
        let mut budget = Budget::new(64 << 10);
        let arena = Arena::new(&mut budget, "row set transfer", 64 << 10).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let encoded = arena
                .with_remaining_bytes(|scratch| {
                    let first = encode_projected_pub(&[Datum::Int4(7), Datum::Null], scratch)?;
                    let second = encode_projected_pub(&[Datum::Text("typed result")], scratch)?;
                    let rows = [first, second, &[]];
                    let output = scratch
                        .alloc_slice_with(projected_rows_len(&rows)?, |_| 0u8)
                        .unwrap();
                    pack_projected_rows_into(&rows, output)?;
                    Ok::<_, SqlError>(&*output)
                })
                .unwrap()
                .unwrap();
            let rows = unpack_projected_rows(encoded, &arena).unwrap();
            arena.alloc_slice_with(4096, |_| 0xcc_u8).unwrap();
            assert_eq!(rows.len(), 3);
            assert_eq!(decode_projected_pub(rows[0], 0), Datum::Int4(7));
            assert_eq!(decode_projected_pub(rows[0], 1), Datum::Null);
            assert_eq!(
                decode_projected_pub(rows[1], 0),
                Datum::Text("typed result")
            );
            assert!(rows[2].is_empty());
            let mut empty = [0u8; 8];
            pack_projected_rows_into(&[], &mut empty).unwrap();
            assert!(unpack_projected_rows(&empty, &arena).unwrap().is_empty());
            for invalid in [&[][..], &[0][..], &[1, 0, 0, 0, 0, 0, 0, 0][..]] {
                assert_eq!(
                    unpack_projected_rows(invalid, &arena).unwrap_err().sqlstate,
                    sqlstate::INTERNAL_ERROR
                );
            }
        });
    }

    #[test]
    fn projected_records_preserve_wide_nested_fields_and_following_columns() {
        use crate::sql::types::{RecordField, oid};

        let mut budget = Budget::new(2 << 20);
        let mut arena = Arena::new(&mut budget, "wide projected records", 2 << 20).unwrap();
        for count in [255, 256, crate::sql::exec::MAX_PROJ] {
            let fields = vec![
                RecordField {
                    form: crate::storage::StoredForm::Inline,
                    name: "field",
                    type_oid: oid::INT4,
                    type_mod: -1,
                    value: Datum::Int4(42)
                };
                count
            ];
            let composite = Datum::Composite {
                slot: crate::storage::TypeCreation::parse(17).unwrap(),
                fields: &fields,
            };
            let nested = [RecordField {
                form: crate::storage::StoredForm::Inline,
                name: "nested",
                type_oid: oid::RECORD,
                type_mod: -1,
                value: composite,
            }];
            let values = [
                Datum::Record(&fields),
                composite,
                Datum::Record(&nested),
                Datum::Int4(99),
            ];
            crate::mem::guard::forbid_alloc(|| {
                let encoded = encode_projected_pub(&values, &arena).unwrap();
                assert_eq!(projected_prefix_len(encoded, values.len()), encoded.len());
                assert_eq!(decode_projected_pub(encoded, 3), Datum::Int4(99));
                let mut restored = [Datum::Null; 4];
                assert_eq!(
                    decode_projected_row_into(encoded, &arena, &mut restored).unwrap(),
                    values.len()
                );
                assert_eq!(restored, values);
                for (index, value) in values.iter().enumerate() {
                    assert_eq!(
                        decode_projected_col_record(encoded, index, &arena).unwrap(),
                        *value
                    );
                }
                let raw = crate::sql::array::build(&[Datum::Record(&fields)], &arena).unwrap();
                let payload = crate::sql::array::element_payloads(raw).next().unwrap();
                assert_eq!(
                    decode_projected_col_record(payload, 0, &arena).unwrap(),
                    values[0]
                );
            });
            arena.reset();
        }
    }

    #[test]
    fn projected_records_keep_their_fields_toast_forms() {
        use crate::sql::types::{RecordField, oid};
        use crate::storage::StoredForm;

        let mut budget = Budget::new(64 << 10);
        let arena = Arena::new(&mut budget, "projected record forms", 64 << 10).unwrap();
        let long = "y".repeat(3000);
        let nested = [RecordField {
            form: StoredForm::Compressed(44),
            name: "inner",
            type_oid: oid::TEXT,
            type_mod: -1,
            value: Datum::Text(&long),
        }];
        let fields = [
            RecordField {
                form: StoredForm::Compressed(126),
                name: "a",
                type_oid: oid::TEXT,
                type_mod: -1,
                value: Datum::Text(&long),
            },
            RecordField {
                form: StoredForm::Inline,
                name: "b",
                type_oid: oid::RECORD,
                type_mod: -1,
                value: Datum::Record(&nested),
            },
        ];
        crate::mem::guard::forbid_alloc(|| {
            let encoded = encode_projected_pub(&[Datum::Record(&fields)], &arena).unwrap();
            let Datum::Record(decoded) = decode_projected_col_record(encoded, 0, &arena).unwrap()
            else {
                panic!("a record decodes as a record");
            };
            assert_eq!(decoded, &fields[..]);
            assert_eq!(decoded[0].form, StoredForm::Compressed(126));
            let Datum::Record(inner) = decoded[1].value else {
                panic!("a nested record decodes as a record");
            };
            assert_eq!(inner[0].form, StoredForm::Compressed(44));
            // The forms are not part of the value: the rendered text and the
            // encoding's comparison ignore them.
            let plain = [RecordField {
                form: StoredForm::Inline,
                ..fields[0]
            }];
            let formed = encode_projected_pub(&[Datum::Record(&fields[..1])], &arena).unwrap();
            let unformed = encode_projected_pub(&[Datum::Record(&plain)], &arena).unwrap();
            assert_eq!(
                decode_projected_pub(formed, 0),
                decode_projected_pub(unformed, 0)
            );
        });
    }

    #[test]
    fn projected_records_preserve_long_field_names_and_reject_unencodable_shapes() {
        use crate::sql::types::{RecordField, oid};

        let mut budget = Budget::new(64 << 10);
        let arena = Arena::new(&mut budget, "projected record names", 64 << 10).unwrap();
        let name = "x".repeat(256);
        let fields = [RecordField {
            form: crate::storage::StoredForm::Inline,
            name: &name,
            type_oid: oid::TEXT,
            type_mod: -1,
            value: Datum::Text("value"),
        }];
        crate::mem::guard::forbid_alloc(|| {
            let encoded = encode_projected_pub(&[Datum::Record(&fields)], &arena).unwrap();
            assert_eq!(
                decode_projected_col_record(encoded, 0, &arena).unwrap(),
                Datum::Record(&fields)
            );
        });
        let too_long = "x".repeat(u16::MAX as usize + 1);
        let fields = [RecordField {
            form: crate::storage::StoredForm::Inline,
            name: &too_long,
            ..fields[0]
        }];
        let error = crate::mem::guard::forbid_alloc(|| {
            encode_projected_pub(&[Datum::Record(&fields)], &arena)
        })
        .unwrap_err();
        assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
        assert!(
            error
                .message
                .as_str()
                .contains("record field count or name")
        );
        let fields = vec![
            RecordField {
                form: crate::storage::StoredForm::Inline,
                name: "x",
                type_oid: oid::INT4,
                type_mod: -1,
                value: Datum::Int4(1)
            };
            u16::MAX as usize + 1
        ];
        let error = crate::mem::guard::forbid_alloc(|| {
            encode_projected_pub(&[Datum::Record(&fields)], &arena)
        })
        .unwrap_err();
        assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
    }

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
    fn low_level_identities_round_trip_through_schema_less_spill_rows() {
        let mut budget = Budget::new(1024);
        let arena = Arena::new(&mut budget, "identity projected row", 128).unwrap();
        let values = [
            Datum::Tid(crate::sql::types::Tid {
                block: u32::MAX,
                offset: u16::MAX,
            }),
            Datum::Cid(u32::MAX),
        ];
        let encoded = encode_projected_pub(&values, &arena).unwrap();
        assert_eq!(projected_row_width(encoded), 2);
        assert_eq!(decode_projected_pub(encoded, 0), values[0]);
        assert_eq!(decode_projected_pub(encoded, 1), values[1]);
    }

    #[test]
    fn persistent_projected_rows_survive_front_rewinds() {
        let mut budget = Budget::new(4096);
        let arena = Arena::new(&mut budget, "persistent projected row", 2048).unwrap();
        let mark = arena.mark();
        let encoded =
            encode_projected_persistent_pub(&[Datum::Text("persistent")], &[], &arena).unwrap();
        let _scratch = arena.alloc_slice_with(512, |_| 0xa5u8).unwrap();
        unsafe { arena.rewind_to(mark) };
        let _overwrite = arena.alloc_slice_with(1024, |_| 0x5au8).unwrap();
        assert_eq!(decode_projected_pub(encoded, 0), Datum::Text("persistent"));
    }

    #[test]
    fn user_type_array_creations_round_trip_through_schema_less_spill_rows() {
        use crate::sql::types::{ArrElem, ColType};

        let mut budget = Budget::new(4096);
        let arena = Arena::new(&mut budget, "user array projected row", 2048).unwrap();
        let values = [
            Datum::Array {
                element: ArrElem::Enum(Some(
                    crate::storage::TypeCreation::parse(9_999_997).unwrap(),
                )),
                raw: b"enum",
            },
            Datum::Array {
                element: ArrElem::Domain {
                    identity: Some(crate::storage::TypeCreation::parse(9_999_998).unwrap()),
                    base_code: ColType::Composite(None).code(),
                    base_identity: Some(crate::storage::TypeCreation::parse(9_999_999).unwrap()),
                },
                raw: b"domain",
            },
            Datum::Array {
                element: ArrElem::Composite(Some(
                    crate::storage::TypeCreation::parse(9_999_999).unwrap(),
                )),
                raw: b"composite",
            },
        ];
        let encoded = encode_projected_pub(&values, &arena).unwrap();
        for (column, expected) in values.iter().enumerate() {
            assert_eq!(decode_projected_pub(encoded, column), *expected);
        }
    }

    #[test]
    fn projected_vectors_preserve_bounds_and_value_identity() {
        let mut budget = Budget::new(4096);
        let arena = Arena::new(&mut budget, "projected vectors", 2048).unwrap();
        let values = [
            Datum::Int2Vector {
                raw: &[1, 0, 254, 255],
                lower_bound: -3,
            },
            Datum::OidVector {
                raw: &[9, 0, 0, 0, 10, 0, 0, 0],
                lower_bound: 8,
            },
        ];
        let encoded = encode_projected_pub(&values, &arena).unwrap();
        for (column, expected) in values.iter().enumerate() {
            assert_eq!(decode_projected_pub(encoded, column), *expected);
        }
    }

    #[test]
    fn structural_decode_preserves_record_fields() {
        use crate::sql::types::{RecordField, oid};

        let mut budget = Budget::new(64 * 1024);
        let arena = Arena::new(&mut budget, "record projected row", 32 * 1024).unwrap();
        let fields = [
            RecordField {
                form: crate::storage::StoredForm::Inline,
                name: "f1",
                type_oid: oid::INT4,
                type_mod: -1,
                value: Datum::Int4(42),
            },
            RecordField {
                form: crate::storage::StoredForm::Inline,
                name: "f2",
                type_oid: oid::TEXT,
                type_mod: -1,
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
