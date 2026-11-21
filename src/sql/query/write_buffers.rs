//! The pages a write of an analyzed statement accesses, in PostgreSQL's
//! `BufferUsage` terms, which its `ModifyTable` node reports: each page of
//! the modeled heap layout (`heap_layout`) and btree layouts it reads or
//! changes.
//!
//! - An inserted tuple is added to the last page (one access); a tuple that
//!   does not fit extends the table by a page, which also reads the full
//!   last page, and the new page counts as dirtied and written, as
//!   `ExtendBufferedRelShared` counts it.
//! - An updated row's old version is read again and locked (two accesses).
//!   The new version stays on the old page while that page has room, else it
//!   is added like an inserted tuple (one more access). A new version that
//!   changes no indexed column and stays on its page is a heap-only tuple.
//! - A deleted row's page is locked (one access).
//! - Each index insertion descends its btree from the root (one access per
//!   level). A unique index whose key the update kept reads the old version
//!   too (`_bt_check_unique`). A leaf an insertion fills splits into a new
//!   page, dirtied and written.
//!
//! Pages already in the table count as dirty since their last write, as they
//! stay in PostgreSQL's shared buffers until a checkpoint: only new pages
//! count as dirtied. A write the model does not cover (an index that is not
//! a plain btree of columns, a foreign table) marks its node unmodeled,
//! which `EXPLAIN` rejects.

use super::instrument::{self, BufferUsage, HeldHeap, Role};
use crate::mem::arena::Arena;
use crate::sql::SqlError;
use crate::sql::types::{ColType, Datum};
use crate::storage::heap_layout::{tuple_length, tuple_space};
use crate::storage::{MAX_COLUMNS, Storage};

/// The indexes of one table a write models.
const MAX_WRITE_INDEXES: usize = 16;

/// A table's indexes as its writes descend them.
#[derive(Clone, Copy)]
pub(crate) struct WriteIndexes {
    indexes: [WriteIndex; MAX_WRITE_INDEXES],
    count: usize,
    /// Every index is one the model covers.
    modeled: bool,
}

/// One btree index's modeled layout.
#[derive(Clone, Copy, Default)]
struct WriteIndex {
    /// The pages one descent reads, the leaf included.
    levels: u32,
    unique: bool,
    /// The entries one full leaf holds.
    capacity: f64,
    entries: f64,
    leaves: f64,
    /// Its key and included columns, by bit.
    columns: u64,
}

/// The modeled layouts of the indexes of the table in `slot`, which holds
/// `rows` rows.
fn write_indexes(storage: &Storage, txid: u32, slot: usize, rows: f64) -> WriteIndexes {
    let mut indexes = WriteIndexes {
        indexes: [WriteIndex::default(); MAX_WRITE_INDEXES],
        count: 0,
        modeled: true,
    };
    crate::sql::catalog::planner_indexes(storage, txid, slot, &mut |index| {
        let keys = &index.columns[..index.n_cols];
        let included = &index.include_columns[..index.n_include_cols];
        if index.method != crate::sql::ast::IndexAccessMethod::Btree
            || index.partial
            || index.expression_keys[..index.n_cols].iter().any(|&key| key)
            || indexes.count == MAX_WRITE_INDEXES
            || keys.iter().chain(included).any(|&column| column >= 64)
        {
            indexes.modeled = false;
            return;
        }
        let mut columns = 0u64;
        let mut width = 0.0;
        for &column in keys.iter().chain(included) {
            columns |= 1 << column;
            width += f64::from(super::index_paths::column_width(
                storage,
                txid,
                slot,
                usize::from(column),
            ));
        }
        let size = super::index_paths::btree_size(rows, width, rows, false, 0.0);
        // A leaf's capacity, from a layout large enough to fill its leaves.
        const FULL: f64 = 1_000_000.0;
        let full = super::index_paths::btree_size(FULL, width, FULL, false, 0.0);
        indexes.indexes[indexes.count] = WriteIndex {
            levels: size.height + 1,
            unique: index.unique,
            capacity: (FULL / full.leaves).max(1.0),
            entries: rows,
            leaves: size.leaves.max(1.0),
            columns,
        };
        indexes.count += 1;
    });
    indexes
}

impl WriteIndexes {
    /// The accesses of inserting a row's entries into every index;
    /// `kept` holds the columns an update did not change, by bit.
    fn insert(&mut self, kept: Option<u64>) -> BufferUsage {
        let mut usage = BufferUsage::default();
        for index in &mut self.indexes[..self.count] {
            usage.shared_hit += u64::from(index.levels);
            if index.unique && kept.is_some_and(|kept| kept & index.columns == index.columns) {
                usage.shared_hit += 1;
            }
            index.entries += 1.0;
            let leaves = (index.entries / index.capacity).ceil();
            if leaves > index.leaves {
                let split = (leaves - index.leaves) as u64;
                index.leaves = leaves;
                usage.shared_hit += split;
                usage.shared_dirtied += split;
                usage.shared_written += split;
            }
        }
        usage
    }
}

/// The accesses of adding a tuple of `length` bytes past the table's last
/// row: the last page, and a new page when the tuple does not fit it.
fn append(held: &mut HeldHeap, length: usize) -> BufferUsage {
    let before = held.packing.pages();
    held.packing.add(length);
    let extended = held.packing.pages() - before;
    BufferUsage {
        shared_hit: 1 + 2 * extended,
        shared_dirtied: extended,
        shared_written: extended,
        ..BufferUsage::default()
    }
}

/// `usage` of a temporary relation, whose pages are local.
const fn localized(usage: BufferUsage) -> BufferUsage {
    BufferUsage {
        shared_hit: 0,
        shared_read: 0,
        shared_dirtied: 0,
        shared_written: 0,
        local_hit: usage.shared_hit,
        local_read: usage.shared_read,
        local_dirtied: usage.shared_dirtied,
        local_written: usage.shared_written,
        temp_read: usage.temp_read,
        temp_written: usage.temp_written,
    }
}

/// Counts in the running write's `ModifyTable` node the accesses `change`
/// makes to the table in `slot`, or marks the node unmodeled when `change`
/// returns `None`.
fn count(
    storage: &Storage,
    txid: u32,
    slot: usize,
    arena: &Arena,
    change: impl FnOnce(&mut HeldHeap, &mut WriteIndexes) -> Option<BufferUsage>,
) -> Result<(), SqlError> {
    if !instrument::buffers_counted() {
        return Ok(());
    }
    let modify = instrument::node(Role::Modify);
    if !modify.measured() {
        return Ok(());
    }
    let definition = storage.table_def(slot, txid);
    let local = definition.persistence == crate::storage::RelationPersistence::Temporary;
    if definition.kind != crate::storage::TableKind::Local {
        modify.unmodeled();
        return Ok(());
    }
    drop(definition);
    let usage = instrument::held_heap(storage, slot, txid, arena, |held| {
        let mut indexes = match held.indexes {
            Some(indexes) => indexes,
            None => write_indexes(storage, txid, slot, held.rows() as f64),
        };
        let usage = if indexes.modeled {
            change(held, &mut indexes)
        } else {
            None
        };
        held.indexes = Some(indexes);
        usage
    })?;
    match usage {
        Some(usage) if local => modify.add_buffers(localized(usage)),
        Some(usage) => modify.add_buffers(usage),
        None => modify.unmodeled(),
    }
    Ok(())
}

/// The heap tuple length of the row `values` of the table in `slot`, the
/// datums in `carried` keeping their stored forms. The pages of a TOAST
/// relation are outside the model, so a row TOAST compresses or moves out of
/// line is not counted.
fn row_length(
    storage: &Storage,
    txid: u32,
    slot: usize,
    values: &[Datum<'_>],
    carried: &crate::sql::toast::CarriedForms,
    arena: &Arena,
) -> Result<usize, SqlError> {
    let mut schema = [ColType::Bool; MAX_COLUMNS];
    let n_columns = storage.table_def(slot, txid).schema(&mut schema);
    let n_columns = n_columns.min(values.len());
    let mut forms = [crate::storage::StoredForm::Inline; MAX_COLUMNS];
    crate::sql::toast::row_forms(
        storage,
        slot,
        txid,
        &values[..n_columns],
        carried,
        &mut forms[..n_columns],
        arena,
    )?;
    inline_row_length(
        &values[..n_columns],
        &schema[..n_columns],
        &forms[..n_columns],
    )
}

/// The heap tuple length of the row `values` whose columns TOAST stores in
/// `forms`.
fn inline_row_length(
    values: &[Datum<'_>],
    schema: &[ColType],
    forms: &[crate::storage::StoredForm],
) -> Result<usize, SqlError> {
    if forms
        .iter()
        .any(|form| *form != crate::storage::StoredForm::Inline)
    {
        return Err(crate::sql_err!(
            crate::sql::eval::sqlstate::FEATURE_NOT_SUPPORTED,
            "EXPLAIN (ANALYZE, BUFFERS) of writing a row TOAST compresses or moves out of line is not supported"
        ));
    }
    Ok(tuple_length(values, schema))
}

/// [`row_length`] of the stored row `bytes`, which records its forms.
fn encoded_row_length<'v>(
    storage: &Storage,
    txid: u32,
    slot: usize,
    bytes: &'v [u8],
    values: &'v mut [Datum<'v>; MAX_COLUMNS],
) -> Result<(usize, &'v [Datum<'v>]), SqlError> {
    let mut schema = [ColType::Bool; MAX_COLUMNS];
    let n_columns = storage.table_def(slot, txid).schema(&mut schema);
    crate::storage::rowenc::decode(bytes, &schema[..n_columns], values)?;
    let mut forms = [crate::storage::StoredForm::Inline; MAX_COLUMNS];
    crate::storage::rowenc::stored_forms(bytes, &schema[..n_columns], &mut forms[..n_columns])?;
    let values = &values[..n_columns];
    let length = inline_row_length(values, &schema[..n_columns], &forms[..n_columns])?;
    Ok((length, values))
}

/// Counts the insertion of the row `values` into the table in `slot`.
pub(crate) fn row_inserted(
    storage: &Storage,
    txid: u32,
    slot: usize,
    values: &[Datum<'_>],
    carried: &crate::sql::toast::CarriedForms,
    arena: &Arena,
) -> Result<(), SqlError> {
    if !instrument::buffers_counted() {
        return Ok(());
    }
    let length = row_length(storage, txid, slot, values, carried, arena)?;
    count(storage, txid, slot, arena, |held, indexes| {
        Some(append(held, length).plus(indexes.insert(None)))
    })
}

/// Counts the update of the row `rowid` of the table in `slot` from `old`
/// to `new`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn row_updated(
    storage: &Storage,
    txid: u32,
    slot: usize,
    rowid: u64,
    old: &[Datum<'_>],
    new: &[Datum<'_>],
    carried: &crate::sql::toast::CarriedForms,
    arena: &Arena,
) -> Result<(), SqlError> {
    if !instrument::buffers_counted() {
        return Ok(());
    }
    let length = row_length(storage, txid, slot, new, carried, arena)?;
    updated_to_length(storage, txid, slot, rowid, old, new, length, arena)
}

/// [`row_updated`] to a new row whose heap tuple takes `length` bytes.
#[allow(clippy::too_many_arguments)]
fn updated_to_length(
    storage: &Storage,
    txid: u32,
    slot: usize,
    rowid: u64,
    old: &[Datum<'_>],
    new: &[Datum<'_>],
    length: usize,
    arena: &Arena,
) -> Result<(), SqlError> {
    let mut kept = 0u64;
    for (column, (old, new)) in old.iter().zip(new).enumerate().take(64) {
        if old == new {
            kept |= 1 << column;
        }
    }
    count(storage, txid, slot, arena, |held, indexes| {
        let mut usage = BufferUsage {
            shared_hit: 2,
            ..BufferUsage::default()
        };
        let stays = held
            .page_of(rowid)
            .is_some_and(|page| held.take_free(page, tuple_space(length)));
        if !stays {
            usage = usage.plus(append(held, length));
        }
        let indexed = indexes.indexes[..indexes.count]
            .iter()
            .any(|index| kept & index.columns != index.columns);
        if !stays || indexed {
            usage = usage.plus(indexes.insert(Some(kept)));
        }
        Some(usage)
    })
}

/// Counts the deletion of a row of the table in `slot`.
pub(crate) fn row_deleted(
    storage: &Storage,
    txid: u32,
    slot: usize,
    arena: &Arena,
) -> Result<(), SqlError> {
    count(storage, txid, slot, arena, |_, _| {
        Some(BufferUsage {
            shared_hit: 1,
            ..BufferUsage::default()
        })
    })
}

/// The row `bytes` of the table in `slot` encodes, decoded into `values`.
/// [`row_inserted`] of the row `bytes` encodes.
pub(crate) fn encoded_row_inserted(
    storage: &Storage,
    txid: u32,
    slot: usize,
    bytes: &[u8],
    arena: &Arena,
) -> Result<(), SqlError> {
    if !instrument::buffers_counted() {
        return Ok(());
    }
    let mut values = [Datum::Null; MAX_COLUMNS];
    let (length, _) = encoded_row_length(storage, txid, slot, bytes, &mut values)?;
    count(storage, txid, slot, arena, |held, indexes| {
        Some(append(held, length).plus(indexes.insert(None)))
    })
}

/// [`row_updated`] to the row `new` encodes.
pub(crate) fn encoded_row_updated(
    storage: &Storage,
    txid: u32,
    slot: usize,
    rowid: u64,
    old: &[Datum<'_>],
    new: &[u8],
    arena: &Arena,
) -> Result<(), SqlError> {
    if !instrument::buffers_counted() {
        return Ok(());
    }
    let mut values = [Datum::Null; MAX_COLUMNS];
    let (length, values) = encoded_row_length(storage, txid, slot, new, &mut values)?;
    updated_to_length(storage, txid, slot, rowid, old, values, length, arena)
}

/// Counts an `INSERT ... ON CONFLICT`'s check of its arbiter for the
/// proposed row (`ExecCheckIndexConstraints`): a descent of a unique index,
/// and the conflicting row's page when it `found` one.
pub(crate) fn conflict_checked(
    storage: &Storage,
    txid: u32,
    slot: usize,
    found: bool,
    arena: &Arena,
) -> Result<(), SqlError> {
    count(storage, txid, slot, arena, |_, indexes| {
        let arbiter = indexes.indexes[..indexes.count]
            .iter()
            .find(|index| index.unique)?;
        Some(BufferUsage {
            shared_hit: u64::from(arbiter.levels) + u64::from(found),
            ..BufferUsage::default()
        })
    })
}
