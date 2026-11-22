//! The form PostgreSQL 18.6 stores each value of a heap tuple in after
//! `heap_toast_insert_or_update`: a tuple longer than
//! `TOAST_TUPLE_THRESHOLD` (2032 bytes) has its widest values compressed
//! with pglz and, while it is still longer than `TOAST_TUPLE_TARGET`, moved
//! to the table's TOAST relation, as `toast_tuple_find_biggest_attribute`
//! picks them.
//! <https://github.com/postgres/postgres/blob/REL_18_6/src/backend/access/heap/heaptoast.c>
//! <https://github.com/postgres/postgres/blob/REL_18_6/src/backend/access/table/toast_helper.c>
//!
//! A value read back from a stored row keeps its form: `toast_tuple_init`
//! keeps an unchanged out-of-line value, and an extended (compressed)
//! value is never compressed again, though it may still move out of line.

use crate::sql::eval::sqlstate::UNDEFINED_COLUMN;
use crate::sql::type_layouts::PhysicalLayout;

/// `SizeofHeapTupleHeader`.
const HEAP_TUPLE_HEADER: usize = 23;
/// `TOAST_TUPLE_THRESHOLD` and `TOAST_TUPLE_TARGET` with 8 kB blocks.
const TOAST_TUPLE_TARGET: usize = 2032;
/// `TOAST_TUPLE_TARGET_MAIN`.
const TOAST_TUPLE_TARGET_MAIN: usize = 8160;
/// `MaxHeapTupleSize` with 8 kB blocks.
pub(crate) const MAX_HEAP_TUPLE_SIZE: usize = 8160;
/// `TOAST_POINTER_SIZE`: a one-byte header and `varatt_external`.
pub(crate) const TOAST_POINTER_SIZE: usize = 18;
/// `VARHDRSZ_COMPRESSED`.
const COMPRESSED_HEADER: usize = 8;
/// `VARATT_SHORT_MAX`.
const SHORT_VARLENA_MAX: usize = 127;

/// How a heap tuple holds one value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ToastedForm {
    /// As `heap_form_tuple` formed it.
    Inline,
    /// pglz-compressed in line: the datum's `VARSIZE`.
    Compressed(usize),
    /// Saved in the TOAST relation (`toast_save_datum`): the bytes saved,
    /// which `pg_column_size` reports, and whether they are compressed.
    External { saved: usize, compressed: bool },
}

/// One column of the row stored.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ToastColumn<'v> {
    /// The column's type layout, with its `attstorage`.
    pub(crate) layout: PhysicalLayout,
    /// The column compresses with pglz (`attcompression` empty or `p`, under
    /// `default_toast_compression = pglz`).
    pub(crate) pglz: bool,
    /// `None` for NULL; else the varlena's data length after its header
    /// and, when known, the data bytes pglz reads.
    pub(crate) value: Option<(usize, Option<&'v [u8]>)>,
    /// The form the value keeps from the stored row it was read from.
    pub(crate) stored: Option<crate::storage::rowenc::StoredForm>,
}

impl From<ToastedForm> for crate::storage::rowenc::StoredForm {
    fn from(form: ToastedForm) -> Self {
        let size = |bytes: usize| u32::try_from(bytes).expect("a varlena is below 1 GB");
        match form {
            ToastedForm::Inline => Self::Inline,
            ToastedForm::Compressed(bytes) => Self::Compressed(size(bytes)),
            ToastedForm::External { saved, compressed } => Self::External {
                saved: size(saved),
                compressed,
            },
        }
    }
}

/// How a datum is held, which `pg_column_size` reads. PL/pgSQL keeps the
/// datum it is given (`datumTransfer`), and a call passes its argument
/// datums on unchanged: an expression's result as the expression formed it,
/// and a value read from a tuple as the tuple holds it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DatumForm {
    /// Formed by an expression: a four-byte varlena header, uncompressed.
    Formed,
    /// Read from a tuple: a short value packed, a stored datum in the TOAST
    /// form it was stored in.
    Stored(crate::storage::rowenc::StoredForm),
    /// Passed on from a source that does not record how it held the datum:
    /// neither its header nor its TOAST form is known.
    Unknown,
}

impl DatumForm {
    /// The header the datum has.
    pub(crate) const fn header(self) -> DatumHeader {
        match self {
            Self::Formed => DatumHeader::Formed,
            Self::Stored(_) => DatumHeader::Tuple,
            Self::Unknown => DatumHeader::Unknown,
        }
    }

    /// The TOAST form of `value` held this way: `None` for a datum formed
    /// anew, which tuple forming would form again.
    pub(crate) fn stored(
        self,
        value: &crate::sql::types::Datum<'_>,
    ) -> Option<crate::storage::rowenc::StoredForm> {
        match self {
            Self::Formed => None,
            Self::Stored(form) => Some(form),
            Self::Unknown => unknown_form(value),
        }
    }

    /// This form as one integer, which [`Self::from_code`] reads back: a
    /// statement's journal keeps it with the value.
    pub(crate) const fn code(self) -> i64 {
        use crate::storage::rowenc::StoredForm;
        match self {
            Self::Formed => -1,
            Self::Unknown => -2,
            Self::Stored(StoredForm::Inline) => 0,
            Self::Stored(StoredForm::Compressed(size)) => 1 << 32 | size as i64,
            Self::Stored(StoredForm::External {
                saved,
                compressed: false,
            }) => 2 << 32 | saved as i64,
            Self::Stored(StoredForm::External {
                saved,
                compressed: true,
            }) => 3 << 32 | saved as i64,
            Self::Stored(StoredForm::Unmodelled) => 4 << 32,
        }
    }

    /// The form [`Self::code`] gave `code`.
    pub(crate) const fn from_code(code: i64) -> Option<Self> {
        use crate::storage::rowenc::StoredForm;
        let size = (code & 0xffff_ffff) as u32;
        Some(match (code, code >> 32) {
            (-1, _) => Self::Formed,
            (-2, _) => Self::Unknown,
            (0, _) => Self::Stored(StoredForm::Inline),
            (_, 1) => Self::Stored(StoredForm::Compressed(size)),
            (_, 2) => Self::Stored(StoredForm::External {
                saved: size,
                compressed: false,
            }),
            (_, 3) => Self::Stored(StoredForm::External {
                saved: size,
                compressed: true,
            }),
            (_, 4) if size == 0 => Self::Stored(StoredForm::Unmodelled),
            _ => return None,
        })
    }

    /// This form after a conversion turned `before` into `after`: a
    /// conversion that formed a new datum holds it as formed.
    pub(crate) fn converted(
        self,
        before: &crate::sql::types::Datum<'_>,
        after: &crate::sql::types::Datum<'_>,
    ) -> Self {
        if same_datum_identity(before, after) || datum_identity(after).is_none() {
            self
        } else {
            Self::Formed
        }
    }
}

/// The forms of arguments that reach a routine without how they held their
/// values: an operator's operands, a cast's value, a support function's
/// inputs, or dynamic SQL's `USING` values.
pub(crate) const UNRECORDED_ARGUMENTS: [DatumForm; crate::storage::MAX_ROUTINE_ARGUMENTS] =
    [DatumForm::Unknown; crate::storage::MAX_ROUTINE_ARGUMENTS];

/// How the results of routine calls are held, by the call expression that
/// gave them. PostgreSQL passes a datum a function returns on unchanged
/// (`SPI_datumTransfer` copies it with its header and TOAST form), so a
/// call's result keeps the form its routine gave it; an expression reading
/// the result finds it here by the calling expression and the datum's
/// identity. Entries live in a fixed thread-local table: the newest result
/// of each call expression replaces the oldest entry when the table is full.
mod call_results {
    use super::DatumForm;
    use core::cell::{Cell, RefCell};

    const CAPACITY: usize = 64;

    #[derive(Clone, Copy)]
    struct Entry {
        call: usize,
        identity: (usize, usize),
        form: DatumForm,
        age: u64,
    }

    const EMPTY: Entry = Entry {
        call: 0,
        identity: (0, 0),
        form: DatumForm::Unknown,
        age: 0,
    };

    thread_local! {
        static ENTRIES: RefCell<[Entry; CAPACITY]> = const { RefCell::new([EMPTY; CAPACITY]) };
        static AGE: Cell<u64> = const { Cell::new(0) };
    }

    pub(super) fn record(call: usize, identity: (usize, usize), form: DatumForm) {
        let age = AGE.with(|age| {
            let next = age.get().wrapping_add(1);
            age.set(next);
            next
        });
        ENTRIES.with_borrow_mut(|table| {
            let at = table
                .iter()
                .position(|entry| entry.call == call)
                .unwrap_or_else(|| {
                    (0..CAPACITY)
                        .min_by_key(|&at| table[at].age)
                        .expect("the table has entries")
                });
            table[at] = Entry {
                call,
                identity,
                form,
                age,
            };
        });
    }

    pub(super) fn form(call: usize, identity: (usize, usize)) -> Option<DatumForm> {
        ENTRIES.with_borrow(|table| {
            table
                .iter()
                .find(|entry| entry.call == call && entry.age != 0)
                .filter(|entry| entry.identity == identity)
                .map(|entry| entry.form)
        })
    }
}

/// Notes that the routine call `call` gave `value` held as `form`.
pub(crate) fn record_call_result(
    call: &crate::sql::ast::Expr<'_>,
    value: &crate::sql::types::Datum<'_>,
    form: DatumForm,
) {
    if let Some(identity) = datum_identity(value) {
        call_results::record(core::ptr::from_ref(call) as usize, identity, form);
    }
}

/// Moves the record of `value`, which the call `from` gave, to `to`.
pub(crate) fn moved_call_result(
    from: &crate::sql::ast::Expr<'_>,
    to: &crate::sql::ast::Expr<'_>,
    value: &crate::sql::types::Datum<'_>,
) {
    if let Some(identity) = datum_identity(value)
        && let Some(form) = call_results::form(core::ptr::from_ref(from) as usize, identity)
    {
        call_results::record(core::ptr::from_ref(to) as usize, identity, form);
    }
}

/// How `value`, which the routine call `call` gave, is held: `Some(None)`
/// when the call is a routine's whose result was not recorded with this
/// datum, `None` when `call` is no routine call or `value` no datum TOAST
/// can hold in another form.
fn call_result_form(
    call: &crate::sql::ast::Expr<'_>,
    value: &crate::sql::types::Datum<'_>,
) -> Option<Option<DatumForm>> {
    let identity = datum_identity(value)?;
    let found = call_results::form(core::ptr::from_ref(call) as usize, identity);
    if found.is_some() || routine_call(call) {
        Some(found)
    } else {
        None
    }
}

/// Whether `expression` calls a function with no built-in signature, whose
/// result is the datum its routine returns.
fn routine_call(expression: &crate::sql::ast::Expr<'_>) -> bool {
    match expression {
        crate::sql::ast::Expr::Call { name, kind, .. } => {
            *kind == crate::sql::ast::CallKind::Routine
                && crate::sql::catalog::builtin_callable_signatures(
                    name.rsplit('.').next().unwrap_or(name),
                )
                .next()
                .is_none()
        }
        _ => false,
    }
}

/// Whether `expression`, the body of a SQL function inlined into its
/// caller, may give one of its parameters' datums unchanged: a parameter, a
/// conversion leaving it as it was, or a branch choosing it.
pub(crate) fn passes_parameter(expression: &crate::sql::ast::Expr<'_>) -> bool {
    use crate::sql::ast::Expr;
    match expression {
        Expr::Param(_) => true,
        Expr::Collate { operand, .. } | Expr::Cast { operand, .. } => passes_parameter(operand),
        Expr::Case {
            whens, otherwise, ..
        } => whens
            .iter()
            .map(|(_, then)| *then)
            .chain(otherwise.iter().copied())
            .any(passes_parameter),
        Expr::Call { name, args, .. } => {
            let base = name.rsplit('.').next().unwrap_or(name);
            ["coalesce", "greatest", "least", "nullif"]
                .iter()
                .any(|passing| base.eq_ignore_ascii_case(passing))
                && args.iter().any(|arg| passes_parameter(arg))
        }
        _ => false,
    }
}

/// The varlena header of a datum a row holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DatumHeader {
    /// Read from a tuple, which packs a short value into a one-byte header.
    Tuple,
    /// Formed by an expression, with a four-byte header.
    Formed,
    /// Not recorded by the row's source.
    Unknown,
}

/// The row's forms depend on bytes or a compression method the model does
/// not have: a value that must be compressed has no known data bytes, or
/// its column compresses with lz4.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Unmodelled;

const fn align_to(offset: usize, alignment: u8) -> usize {
    let by = match alignment {
        b'd' => 8,
        b'i' => 4,
        b's' => 2,
        _ => 1,
    };
    offset.div_ceil(by) * by
}

/// `toast_tuple_init`'s state of one attribute.
#[derive(Clone, Copy)]
struct Attribute {
    /// `tai_size`: the datum's `VARSIZE_ANY`.
    size: usize,
    ignore: bool,
    incompressible: bool,
}

/// The data length of a varlena of `payload` bytes as `heap_form_tuple`
/// stores it uncompressed (`VARSIZE_ANY`).
const fn formed_size(layout: PhysicalLayout, payload: usize) -> usize {
    if layout.storage != b'p' && payload < SHORT_VARLENA_MAX {
        payload + 1
    } else {
        payload + 4
    }
}

/// `heap_compute_data_size` of the row with its values in `forms`.
fn data_size(columns: &[ToastColumn<'_>], forms: &[ToastedForm]) -> usize {
    let mut size = 0usize;
    for (column, form) in columns.iter().zip(forms) {
        let Some((payload, _)) = column.value else {
            continue;
        };
        let layout = column.layout;
        if layout.length > 0 {
            size = align_to(size, layout.alignment) + layout.length as usize;
            continue;
        }
        size = match *form {
            ToastedForm::Inline if formed_size(layout, payload) == payload + 1 => {
                size + payload + 1
            }
            ToastedForm::Inline => align_to(size, layout.alignment) + payload + 4,
            ToastedForm::Compressed(bytes) => align_to(size, layout.alignment) + bytes,
            // A TOAST pointer has a one-byte header and no alignment.
            ToastedForm::External { .. } => size + TOAST_POINTER_SIZE,
        };
    }
    size
}

/// `heap_toast_insert_or_update` of a new row of `columns` into a table
/// with (`has_toast`) or without a TOAST relation: writes each value's form
/// to `forms` and returns the stored tuple's `MAXALIGN`ed length, which
/// `RelationGetBufferForTuple` checks against [`MAX_HEAP_TUPLE_SIZE`].
pub(crate) fn toast_row(
    columns: &[ToastColumn<'_>],
    has_toast: bool,
    forms: &mut [ToastedForm],
) -> Result<usize, Unmodelled> {
    use crate::storage::rowenc::StoredForm;
    let forms = &mut forms[..columns.len()];
    for (form, column) in forms.iter_mut().zip(columns) {
        *form = match column.stored {
            None | Some(StoredForm::Inline) => ToastedForm::Inline,
            Some(StoredForm::Compressed(bytes)) => ToastedForm::Compressed(bytes as usize),
            Some(StoredForm::External { saved, compressed }) => ToastedForm::External {
                saved: saved as usize,
                compressed,
            },
            Some(StoredForm::Unmodelled) => return Err(Unmodelled),
        };
    }
    let has_nulls = columns.iter().any(|column| column.value.is_none());
    let bitmap = if has_nulls {
        columns.len().div_ceil(8)
    } else {
        0
    };
    let header = (HEAP_TUPLE_HEADER + bitmap).div_ceil(8) * 8;
    let length = |forms: &[ToastedForm]| (header + data_size(columns, forms)).div_ceil(8) * 8;
    // `heap_insert` toasts only a tuple past the threshold.
    if header + data_size(columns, forms) <= TOAST_TUPLE_TARGET {
        return Ok(length(forms));
    }
    let mut attributes = [Attribute {
        size: 0,
        ignore: true,
        incompressible: false,
    }; crate::storage::MAX_RELATION_COLUMNS];
    let attributes = &mut attributes[..columns.len()];
    for ((attribute, column), form) in attributes.iter_mut().zip(columns).zip(forms.iter()) {
        if let Some((payload, _)) = column.value
            && column.layout.length == -1
        {
            match *form {
                ToastedForm::Inline => {
                    attribute.size = formed_size(column.layout, payload);
                    attribute.ignore = column.layout.storage == b'p';
                }
                ToastedForm::Compressed(bytes) => {
                    attribute.size = bytes;
                    attribute.incompressible = true;
                    attribute.ignore = false;
                }
                ToastedForm::External { .. } => {}
            }
        }
    }
    let mut state = Toaster {
        columns,
        has_toast,
        forms,
        attributes,
    };
    let target = TOAST_TUPLE_TARGET - header;
    while state.size() > target {
        let Some(biggest) = state.biggest(true, false) else {
            break;
        };
        if columns[biggest].layout.storage == b'x' {
            state.try_compression(biggest)?;
        } else {
            state.attributes[biggest].incompressible = true;
        }
        if state.attributes[biggest].size > target && has_toast {
            state.externalize(biggest);
        }
    }
    while state.size() > target && has_toast {
        let Some(biggest) = state.biggest(false, false) else {
            break;
        };
        state.externalize(biggest);
    }
    while state.size() > target {
        let Some(biggest) = state.biggest(true, true) else {
            break;
        };
        state.try_compression(biggest)?;
    }
    let target = TOAST_TUPLE_TARGET_MAIN - header;
    while state.size() > target && has_toast {
        let Some(biggest) = state.biggest(false, true) else {
            break;
        };
        state.externalize(biggest);
    }
    Ok(length(state.forms))
}

/// Whether TOAST could hold `value` other than as `heap_form_tuple` forms
/// it: pglz compresses no input below 32 bytes, and
/// `toast_tuple_find_biggest_attribute` moves out of line only a value
/// wider than an aligned TOAST pointer (24 bytes).
pub(crate) fn may_be_toasted(value: &crate::sql::types::Datum<'_>) -> bool {
    crate::storage::rowenc::carries_stored_form(value)
        && crate::sql::query::postgres_memory::varlena_payload(None, value)
            .is_none_or(|payload| payload >= 24)
}

/// Whether `after` is the very datum `before` (a conversion that left the
/// value as it was), not a copy formed anew.
pub(crate) fn same_datum_identity(
    before: &crate::sql::types::Datum<'_>,
    after: &crate::sql::types::Datum<'_>,
) -> bool {
    datum_identity(before).is_some_and(|identity| Some(identity) == datum_identity(after))
}

/// The bytes a datum of a type TOAST can store compressed or out of line
/// borrows: its identity. A datum passed on unchanged keeps it; a value
/// formed anew borrows other bytes.
fn datum_identity(value: &crate::sql::types::Datum<'_>) -> Option<(usize, usize)> {
    use crate::sql::types::Datum;
    let bytes: &[u8] = match *value {
        Datum::Text(text)
        | Datum::Bpchar(text)
        | Datum::Xml(text)
        | Datum::JsonPath(text)
        | Datum::Json { text, .. }
        | Datum::Range { text, .. }
        | Datum::Multirange { text, .. }
        | Datum::Geometry { text, .. }
        | Datum::Bit { bits: text, .. } => text.as_bytes(),
        Datum::TsVector(vector) => vector.as_str().as_bytes(),
        Datum::Array { raw, .. } | Datum::Bytea(raw) => raw,
        _ => return None,
    };
    Some((bytes.as_ptr() as usize, bytes.len()))
}

/// The form `after` keeps when a conversion of `before`, a datum holding
/// `form`, gives it: `form` when the conversion passed the datum on
/// unchanged, else `Inline`.
pub(crate) fn converted_form(
    before: &crate::sql::types::Datum<'_>,
    after: &crate::sql::types::Datum<'_>,
    form: crate::storage::rowenc::StoredForm,
) -> crate::storage::rowenc::StoredForm {
    match (datum_identity(before), datum_identity(after)) {
        (Some(before), Some(after)) if before == after => form,
        _ => crate::storage::rowenc::StoredForm::Inline,
    }
}

/// One datum a statement passes on into a row it writes.
#[derive(Clone, Copy)]
struct Carried {
    identity: (usize, usize),
    form: crate::storage::rowenc::StoredForm,
    /// The column of the row being updated that stores the datum.
    column: Option<u16>,
}

/// The stored datums, with their TOAST forms, that a statement may pass on
/// into a row it writes. A written value keeps a form only when it is that
/// very datum, as `toast_tuple_init` sees the datum it is handed: a value a
/// trigger, cast or expression formed anew is formed again.
pub(crate) struct CarriedForms {
    entries: [Carried; crate::storage::MAX_COLUMNS],
    count: usize,
}

impl CarriedForms {
    /// No stored datum: every value is formed anew.
    pub(crate) const NONE: Self = Self {
        entries: [Carried {
            identity: (0, 0),
            form: crate::storage::rowenc::StoredForm::Inline,
            column: None,
        }; crate::storage::MAX_COLUMNS],
        count: 0,
    };

    /// The datums `values` holding `forms` (`Inline` past its end).
    pub(crate) fn of_values(
        values: &[crate::sql::types::Datum<'_>],
        forms: &[crate::storage::rowenc::StoredForm],
    ) -> Result<Self, crate::sql::eval::SqlError> {
        let mut carried = Self::NONE;
        for (value, form) in values.iter().zip(forms) {
            carried.carry(value, *form, None)?;
        }
        Ok(carried)
    }

    /// The datums of the projected row `bytes`, as decoding it gives them,
    /// with the forms its values carry, passed on into a new row.
    pub(crate) fn of_projected(bytes: &[u8]) -> Result<Self, crate::sql::eval::SqlError> {
        let mut carried = Self::NONE;
        for column in 0..crate::sql::exec::projected_row_width(bytes) {
            let form = crate::sql::exec::projected_stored_form(bytes, column);
            if form != crate::storage::rowenc::StoredForm::Inline {
                let value = crate::sql::exec::decode_projected_pub(bytes, column);
                carried.carry(&value, form, None)?;
            }
        }
        Ok(carried)
    }

    /// The datums of the stored row `bytes`, whose values are `values` of
    /// types `schema`: in their columns of the row being updated when
    /// `in_place`, else passed on into a new row.
    pub(crate) fn of_stored_row(
        bytes: &[u8],
        schema: &[crate::sql::types::ColType],
        values: &[crate::sql::types::Datum<'_>],
        in_place: bool,
    ) -> Result<Self, crate::sql::eval::SqlError> {
        let mut forms = [crate::storage::rowenc::StoredForm::Inline; crate::storage::MAX_COLUMNS];
        let forms = &mut forms[..schema.len()];
        crate::storage::rowenc::stored_forms(bytes, schema, forms)?;
        let mut carried = Self::NONE;
        for (column, (value, form)) in values.iter().zip(forms.iter()).enumerate() {
            let column = if in_place {
                u16::try_from(column).ok()
            } else {
                None
            };
            carried.carry(value, *form, column)?;
        }
        Ok(carried)
    }

    /// Notes that `value` is a stored datum in `form`, stored in `column`
    /// of the row being updated.
    pub(crate) fn carry(
        &mut self,
        value: &crate::sql::types::Datum<'_>,
        form: crate::storage::rowenc::StoredForm,
        column: Option<u16>,
    ) -> Result<(), crate::sql::eval::SqlError> {
        if form == crate::storage::rowenc::StoredForm::Inline {
            return Ok(());
        }
        let Some(identity) = datum_identity(value) else {
            return Ok(());
        };
        let entry = self.entries.get_mut(self.count).ok_or_else(|| {
            crate::sql_err!(
                crate::sql::eval::sqlstate::TOO_MANY_COLUMNS,
                "a written row passes on more stored values than a row has columns"
            )
        })?;
        *entry = Carried {
            identity,
            form,
            column,
        };
        self.count += 1;
        Ok(())
    }

    /// These datums after a copy turned the values `before` into `after`
    /// (a trigger's detached row): each copy of a carried datum carries it.
    pub(crate) fn rebased(
        &self,
        before: &[crate::sql::types::Datum<'_>],
        after: &[crate::sql::types::Datum<'_>],
    ) -> Result<Self, crate::sql::eval::SqlError> {
        let mut rebased = Self::NONE;
        for (before, after) in before.iter().zip(after) {
            let Some(identity) = datum_identity(before) else {
                continue;
            };
            for carried in &self.entries[..self.count] {
                if carried.identity == identity {
                    rebased.carry(after, carried.form, carried.column)?;
                }
            }
        }
        Ok(rebased)
    }

    /// These datums passed into a new row rather than kept in their columns
    /// of the row being updated.
    pub(crate) fn moved(mut self) -> Self {
        for carried in &mut self.entries[..self.count] {
            carried.column = None;
        }
        self
    }

    /// The form the datum `value` keeps when stored in column `column`
    /// (`meta`) of the row written: `None` for a value formed anew. A value
    /// saved out of line stays there only in the column of the updated row
    /// that holds it; anywhere else `toast_tuple_init` fetches it back in
    /// line. A value passed into a length-limited `varchar(n)`, `char(n)` or
    /// `bit(n)` column is formed anew by the length coercion unless its
    /// source had the same limit, which the datum does not record: its form
    /// is then not modelled.
    pub(crate) fn form_of(
        &self,
        value: &crate::sql::types::Datum<'_>,
        column: usize,
        meta: &crate::storage::ColumnMeta,
    ) -> Option<crate::storage::rowenc::StoredForm> {
        use crate::sql::types::ColType;
        use crate::storage::rowenc::StoredForm;
        if self.count == 0 {
            return None;
        }
        let identity = datum_identity(value)?;
        let mut found = None;
        for carried in &self.entries[..self.count] {
            if carried.identity != identity {
                continue;
            }
            let in_place = carried.column == u16::try_from(column).ok();
            let form = if in_place {
                carried.form
            } else if meta.type_mod != -1
                && matches!(
                    meta.ctype,
                    ColType::Varchar | ColType::Bpchar | ColType::Bit { .. }
                )
            {
                StoredForm::Unmodelled
            } else {
                carried.form.fetched()
            };
            if found.is_none() || in_place {
                found = Some(form);
            }
        }
        found
    }
}

/// The TOAST form `value`, the value of `expression` over `row`, keeps
/// when it is stored: PostgreSQL passes a stored datum on unchanged through
/// a column reference, a binary-coercible cast, `COALESCE`, `NULLIF`,
/// `GREATEST`, `LEAST` and `CASE`, so the value keeps its stored form; any
/// other expression forms its value anew (`Inline`). A value an aggregate,
/// window function or scalar subquery may have passed on from a row it read
/// is [`StoredForm::Unmodelled`]: those do not carry the forms of their
/// inputs.
pub(crate) fn passed_form<'a>(
    expression: &crate::sql::ast::Expr<'_>,
    row: &dyn crate::sql::eval::ColumnLookup<'a>,
    catalog: Option<&dyn crate::sql::eval::CatalogAccess>,
    value: &crate::sql::types::Datum<'_>,
) -> Result<crate::storage::rowenc::StoredForm, crate::sql::eval::SqlError> {
    use crate::storage::rowenc::StoredForm;
    if !may_be_toasted(value) {
        return Ok(StoredForm::Inline);
    }
    Ok(branch_form(expression, row, catalog, value)?.unwrap_or(StoredForm::Inline))
}

/// How `value`, the result of `expression` over `row`, is held when the
/// expression's result is passed on as it is (a call's argument, a
/// PL/pgSQL simple expression's result): a datum read from a stored row in
/// its stored form, a value the expression formed with a four-byte header.
pub(crate) fn held_form<'a>(
    expression: &crate::sql::ast::Expr<'_>,
    row: &dyn crate::sql::eval::ColumnLookup<'a>,
    catalog: Option<&dyn crate::sql::eval::CatalogAccess>,
    value: &crate::sql::types::Datum<'_>,
) -> Result<DatumForm, crate::sql::eval::SqlError> {
    Ok(match reads_stored_datum(expression, row, value)? {
        Some(true) => DatumForm::Stored(passed_form(expression, row, catalog, value)?),
        Some(false) => DatumForm::Formed,
        None => DatumForm::Unknown,
    })
}

/// Whether a datum passed on unchanged as `passed` gives `value`: a
/// binary-coercible cast from `text` to `character` relabels it.
fn same_datum(passed: &crate::sql::types::Datum<'_>, value: &crate::sql::types::Datum<'_>) -> bool {
    use crate::sql::types::Datum;
    passed == value
        || matches!((passed, value), (Datum::Text(left), Datum::Bpchar(right)) if left == right)
}

/// Where a value an expression gave comes from.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Source {
    /// A datum read from a stored row.
    Stored,
    /// A datum the expression formed.
    Fresh,
    /// Either.
    Unknown,
}

/// Whether `value`, which `expression` gave, is a datum read from a stored
/// row, which keeps the one-byte varlena header tuple forming gave it:
/// `None` when it may be either, as when branches that could give it differ
/// or an aggregate, window function or subquery passes on a datum it read.
pub(crate) fn reads_stored_datum<'a>(
    expression: &crate::sql::ast::Expr<'_>,
    row: &dyn crate::sql::eval::ColumnLookup<'a>,
    value: &crate::sql::types::Datum<'_>,
) -> Result<Option<bool>, crate::sql::eval::SqlError> {
    Ok(match source_of(expression, row, value)? {
        Some(Source::Stored) => Some(true),
        Some(Source::Fresh) | None => Some(false),
        Some(Source::Unknown) => None,
    })
}

/// [`reads_stored_datum`]: `None` when `expression` cannot have given
/// `value`.
fn source_of<'a>(
    expression: &crate::sql::ast::Expr<'_>,
    row: &dyn crate::sql::eval::ColumnLookup<'a>,
    value: &crate::sql::types::Datum<'_>,
) -> Result<Option<Source>, crate::sql::eval::SqlError> {
    use crate::sql::ast::Expr;
    use crate::sql::types::Datum;
    let column = |qualifier: Option<&str>, name: &str| {
        // A whole-row reference forms a new composite datum.
        let Some(read) = column_datum(row, qualifier, name)? else {
            return Ok(Some(Source::Fresh));
        };
        match row.datum_header(qualifier, name) {
            DatumHeader::Formed => return Ok(same_datum(&read, value).then_some(Source::Fresh)),
            DatumHeader::Unknown => {
                return Ok(same_datum(&read, value).then_some(Source::Unknown));
            }
            DatumHeader::Tuple => {}
        }
        // A composite read from a stored row is the stored datum; one a
        // query formed (a whole row, `ROW(...)`) it passes on as formed.
        Ok(match (read, value) {
            (Datum::CompositeText { slot, .. }, Datum::Composite { slot: given, .. })
                if slot == *given =>
            {
                Some(Source::Stored)
            }
            (Datum::Record(_) | Datum::Composite { .. }, _) if same_datum(&read, value) => {
                Some(Source::Fresh)
            }
            _ => same_datum(&read, value).then_some(Source::Stored),
        })
    };
    let branches = |branches: &mut dyn Iterator<Item = &Expr<'_>>| {
        let mut found = None;
        for branch in branches {
            let Some(source) = source_of(branch, row, value)? else {
                continue;
            };
            found = Some(match found {
                Some(seen) if seen != source => Source::Unknown,
                _ => source,
            });
        }
        Ok(found)
    };
    // A routine's result: a function, operator or cast function call.
    if let Some(form) = call_result_form(expression, value) {
        return Ok(Some(match form {
            Some(DatumForm::Stored(_)) => Source::Stored,
            Some(DatumForm::Formed) => Source::Fresh,
            Some(DatumForm::Unknown) | None => Source::Unknown,
        }));
    }
    match expression {
        Expr::Column { qualifier, name } => column(*qualifier, name),
        Expr::SchemaColumn { table, name, .. } => column(Some(table), name),
        // A conversion that leaves the value as it was returns its datum: a
        // binary-coercible cast, or a length coercion the value fits.
        Expr::Collate { operand, .. } | Expr::Cast { operand, .. } => Ok(Some(
            source_of(operand, row, value)?.unwrap_or(Source::Fresh),
        )),
        Expr::Case {
            whens, otherwise, ..
        } => branches(
            &mut whens
                .iter()
                .map(|(_, then)| *then)
                .chain(otherwise.iter().copied()),
        ),
        Expr::Call {
            name, args, over, ..
        } => {
            let base = name.rsplit('.').next().unwrap_or(name);
            let is = |names: &[&str]| names.iter().any(|name| base.eq_ignore_ascii_case(name));
            if over.is_none() && is(&["coalesce"]) {
                let mut reached = args.len();
                for (at, arg) in args.iter().enumerate() {
                    if reads_column(arg) && branch_gives(arg, row, value)? {
                        reached = at + 1;
                        break;
                    }
                }
                branches(&mut args[..reached].iter().copied())
            } else if over.is_none() && is(&["greatest", "least"]) {
                branches(&mut args.iter().copied())
            } else if over.is_none() && is(&["nullif"]) {
                branches(&mut args.iter().take(1).copied())
            } else if is(PASSING_CALLS) && args.iter().any(|arg| may_pass_stored(arg)) {
                Ok(Some(Source::Unknown))
            } else {
                Ok(Some(Source::Fresh))
            }
        }
        Expr::Str(text) => {
            Ok(matches!(value, Datum::Text(given) if given == text).then_some(Source::Fresh))
        }
        Expr::OuterAggregate { call, .. } => source_of(call, row, value),
        Expr::Subquery(query) if subquery_may_pass(query) => Ok(Some(Source::Unknown)),
        // A field of `ROW(...)` is its argument (`eval_const_expressions`
        // drops the `FieldSelect`); any other composite holds its fields as
        // tuple forming packed them.
        Expr::Field { base, field } => {
            if let Some(table) = whole_row_relation(base, row) {
                return column(Some(table), field);
            }
            match row_argument(base, field) {
                Some(argument) => source_of(argument, row, value),
                None => Ok(Some(Source::Stored)),
            }
        }
        _ => Ok(Some(Source::Fresh)),
    }
}

/// The relation `base` names as a whole-row reference (`t.*` or a bare
/// relation name): its field is the relation's column.
pub(crate) fn whole_row_relation<'e>(
    base: &'e crate::sql::ast::Expr<'_>,
    row: &dyn crate::sql::eval::ColumnLookup<'_>,
) -> Option<&'e str> {
    use crate::sql::ast::Expr;
    match base {
        Expr::WholeRow(table) => Some(table),
        Expr::Column {
            qualifier: None,
            name,
        } if row
            .lookup(None, name)
            .is_err_and(|error| error.sqlstate == UNDEFINED_COLUMN) =>
        {
            Some(name)
        }
        _ => None,
    }
}

/// The argument of the row constructor `base` that its field `field` (`f1`,
/// `f2`, ...) reads; `None` when `base` is no row constructor or the field
/// is not one of its plain arguments.
fn row_argument<'e, 'x>(
    base: &'e crate::sql::ast::Expr<'x>,
    field: &str,
) -> Option<&'e crate::sql::ast::Expr<'x>> {
    use crate::sql::ast::Expr;
    let Expr::Call { args, .. } = base else {
        return None;
    };
    if !base.is_row_constructor()
        || args
            .iter()
            .any(|arg| matches!(arg, Expr::WholeRow(_) | Expr::Field { field: "*", .. }))
    {
        return None;
    }
    let digits = field.strip_prefix('f')?;
    if digits.starts_with('0') {
        return None;
    }
    let position = digits
        .parse::<usize>()
        .ok()
        .filter(|position| *position >= 1)?;
    args.get(position - 1).copied()
}

/// The TOAST form the composite `base` holds its field `field` in, when
/// that field's datum is `value`: `None` when the field does not hold it.
fn field_form<'a>(
    base: &crate::sql::ast::Expr<'_>,
    field: &str,
    row: &dyn crate::sql::eval::ColumnLookup<'a>,
    catalog: Option<&dyn crate::sql::eval::CatalogAccess>,
    value: &crate::sql::types::Datum<'_>,
) -> Result<Option<crate::storage::rowenc::StoredForm>, crate::sql::eval::SqlError> {
    use crate::sql::ast::Expr;
    use crate::sql::types::Datum;
    use crate::storage::rowenc::StoredForm;
    // A field of a whole-row reference is the relation's column.
    if let Some(table) = whole_row_relation(base, row) {
        return Ok(Some(
            row.stored_form(Some(table), field)?
                .unwrap_or(StoredForm::Inline),
        ));
    }
    // A field of a stored composite, or of a composite nested in one, holds
    // the form its spelling records by attribute path.
    if let Some(form) = stored_composite_field_form(base, field, row, catalog)? {
        return Ok(form);
    }
    match base {
        Expr::Column { qualifier, name } => match row.lookup(*qualifier, name)? {
            Datum::Record(fields) | Datum::Composite { fields, .. } => Ok(fields
                .iter()
                .find(|candidate| candidate.name == field && same_datum(&candidate.value, value))
                .map(|candidate| candidate.form)),
            _ => Ok(Some(StoredForm::Unmodelled)),
        },
        _ => match row_argument(base, field) {
            // `eval_const_expressions` reads the argument itself.
            Some(argument) => branch_form(argument, row, catalog, value),
            // A function's composite result holds values it formed; a
            // composite any other expression passes on is not modelled.
            None if matches!(base, Expr::Call { .. }) && !base.is_row_constructor() => {
                Ok(Some(StoredForm::Inline))
            }
            None => Ok(Some(StoredForm::Unmodelled)),
        },
    }
}

/// The form of field `field` of `base` when `base` is a column holding a
/// stored composite, or a chain of fields into one: `Some(None)` when the
/// composite has no such field, `None` when `base` is not such a chain.
fn stored_composite_field_form<'a>(
    base: &crate::sql::ast::Expr<'_>,
    field: &str,
    row: &dyn crate::sql::eval::ColumnLookup<'a>,
    catalog: Option<&dyn crate::sql::eval::CatalogAccess>,
) -> Result<Option<Option<crate::storage::rowenc::StoredForm>>, crate::sql::eval::SqlError> {
    use crate::sql::ast::Expr;
    use crate::sql::types::Datum;
    use crate::storage::rowenc::StoredForm;
    // The field names from the column down, innermost last.
    let mut names = [""; 32];
    names[0] = field;
    let mut depth = 1;
    let mut column = base;
    while let Expr::Field { base, field } = column {
        if depth == names.len() {
            return Ok(None);
        }
        names[depth] = field;
        depth += 1;
        column = base;
    }
    let Expr::Column { qualifier, name } = column else {
        return Ok(None);
    };
    let Datum::CompositeText { slot, text, .. } = row.lookup(*qualifier, name)? else {
        return Ok(None);
    };
    if !text.has_forms() {
        return Ok(Some(Some(StoredForm::Inline)));
    }
    let Some(catalog) = catalog else {
        return Ok(Some(Some(StoredForm::Unmodelled)));
    };
    let mut path = [0usize; 32];
    let mut slot = Some(slot);
    for (at, name) in names[..depth].iter().rev().enumerate() {
        let Some(composite) = slot else {
            return Ok(Some(Some(StoredForm::Unmodelled)));
        };
        let Some((attribute, nested)) = catalog.composite_field(composite, name)? else {
            return Ok(Some(None));
        };
        path[at] = attribute;
        slot = nested;
    }
    Ok(Some(Some(
        text.forms()
            .find(|(entry, _)| entry.is(&path[..depth]))
            .map_or(StoredForm::Inline, |(_, form)| form),
    )))
}

/// Aggregates and window functions that return a datum they read from an
/// input row.
const PASSING_CALLS: &[&str] = &[
    "min",
    "max",
    "any_value",
    "mode",
    "percentile_disc",
    "first_value",
    "last_value",
    "nth_value",
    "lag",
    "lead",
];

/// Whether `expression` may give a datum read from a stored row: a column,
/// a conversion leaving it as it was, a branch choosing it, an aggregate or
/// window function passing an input datum on, or a subquery giving one.
fn may_pass_stored(expression: &crate::sql::ast::Expr<'_>) -> bool {
    use crate::sql::ast::Expr;
    match expression {
        Expr::Column { .. } | Expr::SchemaColumn { .. } => true,
        Expr::Collate { operand, .. } | Expr::Cast { operand, .. } => may_pass_stored(operand),
        Expr::Case {
            whens, otherwise, ..
        } => whens
            .iter()
            .map(|(_, then)| *then)
            .chain(otherwise.iter().copied())
            .any(may_pass_stored),
        // A routine may return a datum it was given or read.
        Expr::Call { .. } if routine_call(expression) => true,
        Expr::Call { name, args, .. } => {
            let base = name.rsplit('.').next().unwrap_or(name);
            ["coalesce", "greatest", "least", "nullif"]
                .iter()
                .chain(PASSING_CALLS)
                .any(|passing| base.eq_ignore_ascii_case(passing))
                && args.iter().any(|arg| may_pass_stored(arg))
        }
        Expr::OuterAggregate { call, .. } => may_pass_stored(call),
        Expr::Subquery(query) => subquery_may_pass(query),
        _ => false,
    }
}

/// Whether the scalar subquery `query` may give a datum read from a stored
/// row ([`may_pass_stored`] of its one output).
fn subquery_may_pass(query: &crate::sql::ast::Select<'_>) -> bool {
    match (query.set_body, query.items) {
        (None, [crate::sql::ast::SelectItem::Expr { expression, .. }]) => {
            may_pass_stored(expression)
        }
        _ => true,
    }
}

/// Whether `expression` reads a column as it is stored.
fn reads_column(expression: &crate::sql::ast::Expr<'_>) -> bool {
    use crate::sql::ast::Expr;
    match expression {
        Expr::Column { .. } | Expr::SchemaColumn { .. } => true,
        Expr::Collate { operand, .. } => reads_column(operand),
        Expr::Cast {
            operand, type_mod, ..
        } => *type_mod == -1 && reads_column(operand),
        _ => false,
    }
}

/// The datum column `name` holds, or `None` when an unqualified `name` is
/// no column but a bare relation name, a whole-row reference (`NEW` and
/// `OLD` in a trigger).
fn column_datum<'a>(
    row: &dyn crate::sql::eval::ColumnLookup<'a>,
    qualifier: Option<&str>,
    name: &str,
) -> Result<Option<crate::sql::types::Datum<'a>>, crate::sql::eval::SqlError> {
    match row.lookup(qualifier, name) {
        Ok(read) => Ok(Some(read)),
        Err(error) if qualifier.is_none() && error.sqlstate == UNDEFINED_COLUMN => Ok(None),
        Err(error) => Err(error),
    }
}

/// Whether the column `expression` reads holds `value`.
fn branch_gives<'a>(
    expression: &crate::sql::ast::Expr<'_>,
    row: &dyn crate::sql::eval::ColumnLookup<'a>,
    value: &crate::sql::types::Datum<'_>,
) -> Result<bool, crate::sql::eval::SqlError> {
    use crate::sql::ast::Expr;
    Ok(match expression {
        // A whole-row reference forms its composite anew.
        Expr::Column { qualifier, name } => {
            column_datum(row, *qualifier, name)?.is_some_and(|read| same_datum(&read, value))
        }
        Expr::SchemaColumn { table, name, .. } => {
            same_datum(&row.lookup(Some(table), name)?, value)
        }
        Expr::Collate { operand, .. } | Expr::Cast { operand, .. } => {
            branch_gives(operand, row, value)?
        }
        _ => false,
    })
}

/// The form `value` holds when `expression` gives it: `None` when the
/// expression cannot have given it (a column holding another value),
/// `Inline` for a value it forms itself.
fn branch_form<'a>(
    expression: &crate::sql::ast::Expr<'_>,
    row: &dyn crate::sql::eval::ColumnLookup<'a>,
    catalog: Option<&dyn crate::sql::eval::CatalogAccess>,
    value: &crate::sql::types::Datum<'_>,
) -> Result<Option<crate::storage::rowenc::StoredForm>, crate::sql::eval::SqlError> {
    Ok(given_form(expression, row, catalog, value)?.map(|given| given.form))
}

/// A form a branch of an expression may have given its value in.
#[derive(Clone, Copy)]
struct Given {
    form: crate::storage::rowenc::StoredForm,
    /// The branch read the very datum given, not only an equal value.
    identical: bool,
}

/// [`branch_form`], noting whether the branch read the very datum given.
fn given_form<'a>(
    expression: &crate::sql::ast::Expr<'_>,
    row: &dyn crate::sql::eval::ColumnLookup<'a>,
    catalog: Option<&dyn crate::sql::eval::CatalogAccess>,
    value: &crate::sql::types::Datum<'_>,
) -> Result<Option<Given>, crate::sql::eval::SqlError> {
    use crate::sql::ast::Expr;
    use crate::storage::rowenc::StoredForm;
    let formed = |form| {
        Ok(Some(Given {
            form,
            identical: false,
        }))
    };
    let column = |qualifier: Option<&str>, name: &str| {
        // A whole-row reference forms its composite datum anew.
        let Some(read) = column_datum(row, qualifier, name)? else {
            return formed(StoredForm::Inline);
        };
        // A stored composite reads materialized with its fields.
        let materialized = matches!(
            (&read, value),
            (
                crate::sql::types::Datum::CompositeText { slot, .. },
                crate::sql::types::Datum::Composite { slot: given, .. },
            ) if slot == given
        );
        if !materialized && !same_datum(&read, value) {
            return Ok(None);
        }
        let identity = datum_identity(value);
        Ok(Some(Given {
            form: row
                .stored_form(qualifier, name)?
                .unwrap_or(StoredForm::Inline),
            identical: identity.is_some() && identity == datum_identity(&read),
        }))
    };
    // The branches the value may come from: their common form, or
    // `Unmodelled` when two that could give it hold different forms. A
    // branch that read the very datum given rules out those that only hold
    // an equal value.
    let branches = |branches: &mut dyn Iterator<Item = &Expr<'_>>| {
        let mut found: Option<Given> = None;
        for branch in branches {
            let Some(given) = given_form(branch, row, catalog, value)? else {
                continue;
            };
            found = Some(match found {
                Some(seen) if seen.identical && !given.identical => seen,
                Some(seen) if seen.identical == given.identical && seen.form != given.form => {
                    Given {
                        form: StoredForm::Unmodelled,
                        identical: seen.identical,
                    }
                }
                _ => given,
            });
        }
        Ok(found)
    };
    // A routine's result is the very datum it returned.
    if let Some(form) = call_result_form(expression, value) {
        return Ok(Some(match form {
            Some(DatumForm::Stored(form)) => Given {
                form,
                identical: true,
            },
            Some(DatumForm::Formed) => Given {
                form: StoredForm::Inline,
                identical: false,
            },
            Some(DatumForm::Unknown) | None => Given {
                form: StoredForm::Unmodelled,
                identical: false,
            },
        }));
    }
    match expression {
        Expr::Column { qualifier, name } => column(*qualifier, name),
        Expr::SchemaColumn { table, name, .. } => column(Some(table), name),
        Expr::Collate { operand, .. } => given_form(operand, row, catalog, value),
        // A cast without a length coercion that leaves the datum as it was
        // is binary coercible (`RelabelType`) or a domain's check.
        Expr::Cast {
            operand, type_mod, ..
        } if *type_mod == -1 => given_form(operand, row, catalog, value),
        Expr::Case {
            whens, otherwise, ..
        } => branches(
            &mut whens
                .iter()
                .map(|(_, then)| *then)
                .chain(otherwise.iter().copied()),
        ),
        // A literal gives only its own value.
        Expr::Str(text) => {
            if matches!(value, crate::sql::types::Datum::Text(given) if given == text) {
                formed(StoredForm::Inline)
            } else {
                Ok(None)
            }
        }
        Expr::Call {
            name, args, over, ..
        } => {
            let base = name.rsplit('.').next().unwrap_or(name);
            let is = |names: &[&str]| names.iter().any(|name| base.eq_ignore_ascii_case(name));
            if over.is_none() && is(&["coalesce"]) {
                // A column that holds the value is the first argument not
                // null: the arguments after it are not evaluated.
                let mut reached = args.len();
                for (at, arg) in args.iter().enumerate() {
                    if reads_column(arg) && branch_gives(arg, row, value)? {
                        reached = at + 1;
                        break;
                    }
                }
                branches(&mut args[..reached].iter().copied())
            } else if over.is_none() && is(&["greatest", "least"]) {
                branches(&mut args.iter().copied())
            } else if over.is_none() && is(&["nullif"]) {
                branches(&mut args.iter().take(1).copied())
            } else if is(PASSING_CALLS) && args.iter().any(|arg| may_pass_stored(arg)) {
                formed(StoredForm::Unmodelled)
            } else {
                formed(StoredForm::Inline)
            }
        }
        Expr::OuterAggregate { call, .. } => given_form(call, row, catalog, value),
        Expr::Subquery(query) if subquery_may_pass(query) => formed(StoredForm::Unmodelled),
        Expr::Field { base, field } => Ok(field_form(base, field, row, catalog, value)?.map(
            |form| Given {
                form,
                identical: false,
            },
        )),
        _ => formed(StoredForm::Inline),
    }
}

/// [`converted_form`] of each value of a row a conversion turned from
/// `before`, holding `forms` (`Inline` past its end), into `after`.
pub(crate) fn converted_forms(
    before: &[crate::sql::types::Datum<'_>],
    after: &[crate::sql::types::Datum<'_>],
    forms: &[crate::storage::rowenc::StoredForm],
) -> [crate::storage::rowenc::StoredForm; crate::sql::exec::MAX_PROJ] {
    let mut kept = [crate::storage::rowenc::StoredForm::Inline; crate::sql::exec::MAX_PROJ];
    for (at, slot) in kept
        .iter_mut()
        .enumerate()
        .take(after.len().min(before.len()))
    {
        if let Some(form) = forms.get(at) {
            *slot = converted_form(&before[at], &after[at], *form);
        }
    }
    kept
}

/// The form of `value` when it is the very datum of one of `values`, whose
/// stored forms are `forms`.
pub(crate) fn form_among(
    value: &crate::sql::types::Datum<'_>,
    values: &[crate::sql::types::Datum<'_>],
    forms: &[crate::storage::rowenc::StoredForm],
) -> Option<crate::storage::rowenc::StoredForm> {
    let identity = datum_identity(value)?;
    values
        .iter()
        .zip(forms)
        .find(|(candidate, _)| datum_identity(candidate) == Some(identity))
        .map(|(_, form)| *form)
}

/// The form of `value`, read from a row that does not keep its values'
/// stored forms: one TOAST may have compressed or moved out of line is
/// [`StoredForm::Unmodelled`](crate::storage::rowenc::StoredForm).
pub(crate) fn unknown_form(
    value: &crate::sql::types::Datum<'_>,
) -> Option<crate::storage::rowenc::StoredForm> {
    may_be_toasted(value).then_some(crate::storage::rowenc::StoredForm::Unmodelled)
}

/// The form a result row that does not carry its values' forms gives
/// `value`: one TOAST may have stored otherwise is not modelled, and a
/// shorter one is held as tuple forming packs it.
pub(crate) fn uncarried_form(
    value: &crate::sql::types::Datum<'_>,
) -> crate::storage::rowenc::StoredForm {
    unknown_form(value).unwrap_or(crate::storage::rowenc::StoredForm::Inline)
}

/// The form `value`, a grouped query's output of `expression`, keeps: a
/// grouping key is a datum of an input row whose form the grouping does not
/// keep, so a key TOAST may have stored otherwise is
/// [`StoredForm::Unmodelled`](crate::storage::rowenc::StoredForm).
pub(crate) fn grouped_form<'a>(
    expression: &crate::sql::ast::Expr<'_>,
    value: &crate::sql::types::Datum<'a>,
) -> Result<crate::storage::rowenc::StoredForm, crate::sql::eval::SqlError> {
    passed_form(expression, &GroupingKey(*value), None, value)
}

/// The row a grouped query's output `.0` reads: every column it reads is a
/// grouping key, whose value it is, and whose stored form the grouping does
/// not keep.
pub(crate) struct GroupingKey<'v>(pub(crate) crate::sql::types::Datum<'v>);

impl<'v> crate::sql::eval::ColumnLookup<'v> for GroupingKey<'v> {
    fn lookup(
        &self,
        _qualifier: Option<&str>,
        _name: &str,
    ) -> Result<crate::sql::types::Datum<'v>, crate::sql::eval::SqlError> {
        Ok(self.0)
    }

    fn stored_form(
        &self,
        _qualifier: Option<&str>,
        _name: &str,
    ) -> Result<Option<crate::storage::rowenc::StoredForm>, crate::sql::eval::SqlError> {
        Ok(unknown_form(&self.0))
    }
}

/// Whether a stored datum of the data `bytes` may be compressed:
/// `toast_compress_datum` keeps a result, after its 8-byte header, more than
/// two bytes shorter than the data. pglz is sized exactly. lz4
/// (`LZ4_compress_default`) starts a match at a four-byte sequence seen
/// before, from the second byte to 12 bytes (`MFLIMIT`) from the end, and
/// ends it 5 bytes (`LASTLITERALS`) from the end; its output holds every
/// byte no match covers, plus a token and a 2-byte offset per match and a
/// final token. So it saves enough only when matches can cover at least 15
/// bytes. A value past 512 bytes is not searched and may be compressed.
fn may_compress(bytes: &[u8]) -> bool {
    let n = bytes.len();
    if crate::sql::pglz::compressed_size(bytes)
        .is_some_and(|length| length + COMPRESSED_HEADER < n.saturating_sub(2))
    {
        return true;
    }
    if n > 512 {
        return true;
    }
    let Some(last_start) = n.checked_sub(12) else {
        return false;
    };
    let end = n - 5;
    let mut covered = [false; 512];
    for start in 1..=last_start {
        for earlier in 0..start {
            let length = (0..end - start)
                .take_while(|&at| bytes[earlier + at] == bytes[start + at])
                .count();
            if length >= 4 {
                covered[start..start + length].fill(true);
            }
        }
    }
    covered.iter().filter(|&&covered| covered).count() >= 15
}

/// How `toast_compress_datum` treats a value of `payload` data bytes.
pub(crate) enum Compression {
    /// Compressed: the datum's `VARSIZE`.
    Compressed(usize),
    /// pglz keeps no result short enough.
    Incompressible,
    /// The bytes pglz reads of the value are not modelled.
    Unknown,
}

/// [`Compression`] of `value` under pglz, the default
/// `default_toast_compression`.
pub(crate) fn pglz_compression(
    value: &crate::sql::types::Datum<'_>,
    payload: usize,
    arena: &crate::mem::arena::Arena,
) -> Result<Compression, crate::sql::eval::SqlError> {
    let Some(bytes) = pglz_input(value, arena)? else {
        return Ok(Compression::Unknown);
    };
    Ok(match crate::sql::pglz::compressed_size(bytes) {
        Some(length) if length + COMPRESSED_HEADER < payload.saturating_sub(2) => {
            Compression::Compressed(length + COMPRESSED_HEADER)
        }
        _ => Compression::Incompressible,
    })
}

/// The bytes pglz reads of `value` as PostgreSQL stores it: the data of the
/// types pos3ql holds byte for byte, or formed into `scratch` for a tsvector,
/// path or polygon. `None` for a type whose stored data is not modelled.
fn pglz_input<'v>(
    value: &crate::sql::types::Datum<'v>,
    scratch: &'v crate::mem::arena::Arena,
) -> Result<Option<&'v [u8]>, crate::sql::eval::SqlError> {
    use crate::sql::types::Datum;
    Ok(match *value {
        Datum::Text(text) | Datum::Bpchar(text) | Datum::Xml(text) => Some(text.as_bytes()),
        Datum::Json { text, jsonb: false } => Some(text.as_bytes()),
        Datum::Bytea(bytes) => Some(bytes),
        Datum::TsVector(vector) => Some(crate::sql::full_text::vector_stored_data(
            vector.as_str(),
            scratch,
        )?),
        Datum::Geometry { kind, text } => crate::sql::geometry::stored_data(kind, text, scratch)?,
        _ => None,
    })
}

/// The form each of `values`, a new row of the table in slot `table`,
/// takes when stored (`heap_toast_insert_or_update`), into `forms`.
///
/// A datum in `carried` the statement passes on into the row keeps its
/// form ([`CarriedForms::form_of`]); the statement formed every other
/// value. A row the model cannot size marks its varlenas
/// [`StoredForm::Unmodelled`].
pub(crate) fn row_forms(
    storage: &crate::storage::Storage,
    table: usize,
    txid: u32,
    values: &[crate::sql::types::Datum<'_>],
    carried: &CarriedForms,
    forms: &mut [crate::storage::rowenc::StoredForm],
    arena: &crate::mem::arena::Arena,
) -> Result<(), crate::sql::eval::SqlError> {
    let definition = storage.table_def(table, txid);
    row_forms_as(
        definition.columns(),
        definition.has_toast,
        &crate::sql::query::postgres_memory::Sizing {
            catalog: Some(&crate::sql::query::storage_catalog(storage, arena, txid)),
            scratch: arena,
        },
        values,
        carried,
        forms,
    )
}

/// [`row_forms`] of a row of a table with the columns `metas`, which has a
/// TOAST relation when `has_toast`: a table a rewrite is still forming.
pub(crate) fn row_forms_as(
    metas: &[crate::storage::ColumnMeta],
    has_toast: bool,
    sizing: &crate::sql::query::postgres_memory::Sizing<'_>,
    values: &[crate::sql::types::Datum<'_>],
    carried: &CarriedForms,
    forms: &mut [crate::storage::rowenc::StoredForm],
) -> Result<(), crate::sql::eval::SqlError> {
    use crate::sql::ast::ColumnCompression;
    use crate::storage::rowenc::{StoredForm, carries_stored_form};
    let forms = &mut forms[..values.len()];
    forms.fill(StoredForm::Inline);
    // A row that cannot reach the threshold even with four-byte headers and
    // full alignment, and keeps no stored form, stays in line.
    if carried.count == 0 {
        let mut bound = (HEAP_TUPLE_HEADER + values.len().div_ceil(8)).div_ceil(8) * 8;
        let mut sized = true;
        for value in values.iter().filter(|value| !value.is_null()) {
            match crate::sql::query::postgres_memory::stored(Some(sizing), None, value) {
                Some(stored) if stored.layout.length > 0 => {
                    bound += stored.layout.length as usize + 7;
                }
                Some(stored) => bound += stored.payload + 4 + 7,
                None => {
                    sized = false;
                    break;
                }
            }
        }
        if sized && bound <= TOAST_TUPLE_TARGET {
            return Ok(());
        }
    }
    let mut columns = [ToastColumn {
        layout: PhysicalLayout {
            length: 4,
            by_value: true,
            alignment: b'i',
            storage: b'p',
        },
        pglz: true,
        value: None,
        stored: None,
    }; crate::storage::MAX_RELATION_COLUMNS];
    let mut modelled = values.len() == metas.len() && values.len() <= columns.len();
    if modelled {
        for (at, value) in values.iter().enumerate() {
            if value.is_null() {
                continue;
            }
            let Some(stored) =
                crate::sql::query::postgres_memory::stored(Some(sizing), None, value)
            else {
                modelled = false;
                break;
            };
            let mut layout = stored.layout;
            if layout.length == -1 {
                layout.storage = metas[at].storage.code();
            }
            let bytes = pglz_input(value, sizing.scratch)?;
            columns[at] = ToastColumn {
                layout,
                pglz: metas[at].compression != ColumnCompression::Lz4,
                value: Some((stored.payload, bytes)),
                // `detoast_attr` gives a plain column its value uncompressed.
                // A datum of unknown form that no method compresses reaches
                // the row in line: `toast_tuple_init` fetches a value saved
                // out of line back before toasting the row.
                stored: carried
                    .form_of(value, at, &metas[at])
                    .filter(|_| layout.storage != b'p')
                    .filter(|form| {
                        *form != StoredForm::Unmodelled || bytes.is_none_or(may_compress)
                    }),
            };
        }
    }
    let mut toasted = [ToastedForm::Inline; crate::storage::MAX_RELATION_COLUMNS];
    let result = if modelled {
        toast_row(
            &columns[..values.len()],
            has_toast,
            &mut toasted[..values.len()],
        )
    } else {
        Err(Unmodelled)
    };
    if let Ok(length) = result
        && length > MAX_HEAP_TUPLE_SIZE
    {
        return Err(crate::sql_err!(
            crate::sql::eval::sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "row is too big: size {}, maximum size {}",
            length,
            MAX_HEAP_TUPLE_SIZE
        ));
    }
    for (at, (form, value)) in forms.iter_mut().zip(values).enumerate() {
        let toasted = match result {
            Ok(_) => StoredForm::from(toasted[at]),
            // An unsized row's forms are unknown, save that a value too
            // short to compress or move out of line stays in line.
            Err(Unmodelled) if may_be_toasted(value) => StoredForm::Unmodelled,
            Err(Unmodelled) => StoredForm::Inline,
        };
        if toasted == StoredForm::Inline || value.is_null() {
            continue;
        }
        // The row encoding records a form only on the types whose varlena
        // TOAST usually reaches; a `numeric`, `inet` or composite value TOAST
        // would store otherwise is outside the model.
        if !carries_stored_form(value) {
            return Err(crate::sql_err!(
                crate::sql::eval::sqlstate::FEATURE_NOT_SUPPORTED,
                "storing a row whose TOAST compresses or moves out of line a value of this type is not supported"
            ));
        }
        *form = toasted;
    }
    Ok(())
}

/// `ToastTupleContext` of one row.
struct Toaster<'r, 'v> {
    columns: &'r [ToastColumn<'v>],
    has_toast: bool,
    forms: &'r mut [ToastedForm],
    attributes: &'r mut [Attribute],
}

impl Toaster<'_, '_> {
    fn size(&self) -> usize {
        data_size(self.columns, self.forms)
    }

    /// `toast_tuple_find_biggest_attribute`: the widest value wider than a
    /// TOAST pointer still to compress, or to move out of line, among the
    /// `main` columns when `main`, else the `extended` and `external` ones.
    fn biggest(&self, for_compression: bool, main: bool) -> Option<usize> {
        let mut biggest = None;
        let mut biggest_size = TOAST_POINTER_SIZE.div_ceil(8) * 8;
        for (at, (attribute, column)) in self.attributes.iter().zip(self.columns).enumerate() {
            if attribute.ignore
                || (for_compression && attribute.incompressible)
                || matches!(self.forms[at], ToastedForm::External { .. })
                || (for_compression && matches!(self.forms[at], ToastedForm::Compressed(_)))
            {
                continue;
            }
            let storage = column.layout.storage;
            if (main && storage != b'm') || (!main && storage != b'x' && storage != b'e') {
                continue;
            }
            if attribute.size > biggest_size {
                biggest = Some(at);
                biggest_size = attribute.size;
            }
        }
        biggest
    }

    /// `toast_tuple_try_compression`: pglz must save more than two bytes
    /// (`toast_compress_datum`).
    fn try_compression(&mut self, at: usize) -> Result<(), Unmodelled> {
        let column = self.columns[at];
        let (payload, bytes) = column.value.ok_or(Unmodelled)?;
        if !column.pglz {
            return Err(Unmodelled);
        }
        let bytes = bytes.ok_or(Unmodelled)?;
        match crate::sql::pglz::compressed_size(bytes) {
            Some(length) if length + COMPRESSED_HEADER < payload.saturating_sub(2) => {
                self.forms[at] = ToastedForm::Compressed(length + COMPRESSED_HEADER);
                self.attributes[at].size = length + COMPRESSED_HEADER;
            }
            _ => self.attributes[at].incompressible = true,
        }
        Ok(())
    }

    /// `toast_tuple_externalize`: the value, compressed or not, is saved in
    /// the TOAST relation and the tuple keeps a pointer.
    fn externalize(&mut self, at: usize) {
        let payload = self.columns[at].value.map_or(0, |(payload, _)| payload);
        self.forms[at] = match self.forms[at] {
            ToastedForm::Compressed(bytes) => ToastedForm::External {
                saved: bytes - 4,
                compressed: true,
            },
            _ => ToastedForm::External {
                saved: payload,
                compressed: false,
            },
        };
        self.attributes[at].ignore = true;
        debug_assert!(self.has_toast);
    }
}

#[cfg(all(test, not(feature = "sql-fixture-suite")))]
mod tests {
    use super::*;

    const TEXT: PhysicalLayout = PhysicalLayout {
        length: -1,
        by_value: false,
        alignment: b'i',
        storage: b'x',
    };
    const INT: PhysicalLayout = PhysicalLayout {
        length: 4,
        by_value: true,
        alignment: b'i',
        storage: b'p',
    };

    fn text(bytes: &[u8]) -> ToastColumn<'_> {
        ToastColumn {
            layout: TEXT,
            pglz: true,
            value: Some((bytes.len(), Some(bytes))),
            stored: None,
        }
    }

    #[test]
    fn only_values_some_method_compresses_may_be_compressed() {
        // A repeat that starts within lz4's last 12 bytes is no match, one
        // cut short 5 bytes from the end saves too little, and pglz saves
        // too little on either.
        assert!(!may_compress(b"walk_t_a_not_null on public.walk_t"));
        assert!(!may_compress(b"public.walk_f(public.walk_c)"));
        assert!(!may_compress(b"abcdefghijklmnopqrstuvwxyz0123456789"));
        // lz4 matches cover 15 bytes or more.
        assert!(may_compress(
            b"abcdefghijklmnop-abcdefghijklmnop-qrstuvwxyz"
        ));
        assert!(may_compress(&[b'x'; 64]));
        assert!(may_compress(&[b'y'; 600]));
    }

    #[test]
    fn rows_past_the_threshold_compress_their_widest_values() {
        // `pg_column_size` on PostgreSQL 18.6: a 3000-byte run compresses
        // to 44 bytes; two 1500-byte values of one row compress (the row
        // passes 2032 bytes), one alone stays in line.
        let run = [b'x'; 3000];
        let id = ToastColumn {
            layout: INT,
            pglz: true,
            value: Some((0, None)),
            stored: None,
        };
        let mut forms = [ToastedForm::Inline; 2];
        toast_row(&[id, text(&run)], true, &mut forms).unwrap();
        assert_eq!(forms[1], ToastedForm::Compressed(44));
        let half = [b'y'; 1500];
        let mut forms = [ToastedForm::Inline; 2];
        toast_row(&[text(&half), text(&half)], true, &mut forms).unwrap();
        assert_eq!(forms, [ToastedForm::Compressed(28), ToastedForm::Inline]);
        let mut forms = [ToastedForm::Inline; 1];
        toast_row(&[text(&half)], true, &mut forms).unwrap();
        assert_eq!(forms, [ToastedForm::Inline]);
        // Incompressible data past the target moves out of line.
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let noise: [u8; 4000] = core::array::from_fn(|_| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed >> 24) as u8
        });
        let mut forms = [ToastedForm::Inline; 1];
        toast_row(&[text(&noise)], true, &mut forms).unwrap();
        assert_eq!(
            forms,
            [ToastedForm::External {
                saved: 4000,
                compressed: false
            }]
        );
        // Without the data bytes a value that must be compressed is not
        // modelled.
        let unknown = ToastColumn {
            layout: TEXT,
            pglz: true,
            value: Some((3000, None)),
            stored: None,
        };
        assert_eq!(toast_row(&[unknown], true, &mut forms), Err(Unmodelled));
    }
}
