//! Partition clones of foreign keys that reference partitioned tables.
//!
//! PostgreSQL's `addFkRecurseReferenced` gives a foreign key one extra
//! `pg_constraint` row, with its own two RI action triggers, for every
//! partition (at any depth) of the partitioned table it references. Each clone
//! is named by `ChooseConstraintName` against the referencing table's
//! namespace when it is made, and keeps that name. The clones live on the
//! referencing table's definition; after every table DDL statement they are
//! reconciled with the partition trees, so creating, attaching, detaching or
//! dropping a partition, or adding or dropping a key, adds and removes exactly
//! the clones PostgreSQL would, in its creation order.

use super::*;

/// Brings every referencing table's clones in line with the partition trees
/// its foreign keys reference. Existing clones keep their identity and name.
/// A key the statement created (stamped after `statement_stamp`) gets its
/// check triggers after its first clones, as `addFkConstraint` creates them.
pub(crate) fn reconcile_foreign_key_clones(
    storage: &mut Storage,
    wal: &mut Wal,
    txn: &mut TxnState,
    undo_mark: usize,
    statement_stamp: u64,
    arena: &Arena,
) -> Result<(), SqlError> {
    use crate::sql::txn::DdlUndo;
    if !txn.ddl()[undo_mark..].iter().any(|undo| {
        matches!(
            undo,
            DdlUndo::Created(_) | DdlUndo::Dropped(_) | DdlUndo::TableAltered(_)
        )
    }) {
        return Ok(());
    }
    let tables = storage.table_count();
    let order = arena
        .alloc_slice_with(tables, |_| 0usize)
        .map_err(|_| crate::sql::eval::arena_full())?;
    let scratch = arena
        .alloc_slice_with(tables, |_| 0usize)
        .map_err(|_| crate::sql::eval::arena_full())?;
    for table in 0..tables {
        if !storage.relation_visible_to_current_session(table, txn.txid) {
            continue;
        }
        let current = storage.table_def(table, txn.txid);
        if current.fkeys().is_empty() && current.fkey_clones().is_empty() {
            continue;
        }
        if clones_current(storage, txn.txid, table, &current, order, scratch)? {
            continue;
        }
        let mut next = TableDefBuilder::from_definition(&current);
        next.n_fkey_clones = 0;
        // Clones that are still wanted keep their place and identity.
        for clone in current.fkey_clones() {
            let Some(key) = current
                .fkeys()
                .iter()
                .find(|key| key.identity == Some(clone.foreign_key))
            else {
                continue;
            };
            if clone_wanted(storage, txn.txid, table, key, clone) {
                next.fkey_clones[next.n_fkey_clones] = *clone;
                next.n_fkey_clones += 1;
            }
        }
        for key_index in 0..current.fkeys().len() {
            let key = current.fkeys()[key_index];
            let (Some(identity), Some(referenced)) = (
                key.identity,
                cloned_referenced_table(storage, txn.txid, table, &key),
            ) else {
                continue;
            };
            let first_clones = identity.created_at() > statement_stamp
                && key.check_triggers == Some(identity)
                && !current
                    .fkey_clones()
                    .iter()
                    .any(|clone| clone.foreign_key == identity);
            let mut count = 0usize;
            partitions_in_bound_order(storage, txn.txid, referenced, order, &mut count, scratch)?;
            for &partition in &order[..count] {
                let position = storage.table_position(partition);
                if next.fkey_clones[..next.n_fkey_clones]
                    .iter()
                    .any(|clone| clone.foreign_key == identity && clone.partition == position)
                {
                    continue;
                }
                if next.n_fkey_clones == crate::storage::MAX_FKEY_CLONES {
                    return Err(sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "foreign keys of table \"{}\" can reference at most {} partitions",
                        current.name.as_str(),
                        crate::storage::MAX_FKEY_CLONES
                    ));
                }
                let name = clone_name(storage, &next, key.name.as_str(), table, txn.txid)?;
                next.fkey_clones[next.n_fkey_clones] = crate::storage::ForeignKeyClone {
                    identity: None,
                    foreign_key: identity,
                    partition: position,
                    name,
                };
                next.n_fkey_clones += 1;
                if first_clones {
                    // Stamped again once the clones are.
                    next.fkeys[key_index].check_triggers = None;
                }
            }
        }
        drop(current);
        storage.lock_table(
            txn.txid,
            table,
            crate::sql::ast::TableLockMode::ShareRowExclusive,
            false,
        )?;
        let (target, previous) = storage.select_table_definition(table, txn.txid);
        super::stage_table_definition(storage, wal, txn, target, previous, next)?;
    }
    Ok(())
}

/// Whether `current`'s clones are exactly the ones its keys need now.
fn clones_current(
    storage: &Storage,
    txid: u32,
    table: usize,
    current: &TableDef,
    order: &mut [usize],
    scratch: &mut [usize],
) -> Result<bool, SqlError> {
    for clone in current.fkey_clones() {
        let Some(key) = current
            .fkeys()
            .iter()
            .find(|key| key.identity == Some(clone.foreign_key))
        else {
            return Ok(false);
        };
        if !clone_wanted(storage, txid, table, key, clone) {
            return Ok(false);
        }
    }
    for key in current.fkeys() {
        let (Some(identity), Some(referenced)) = (
            key.identity,
            cloned_referenced_table(storage, txid, table, key),
        ) else {
            continue;
        };
        let mut count = 0usize;
        partitions_in_bound_order(storage, txid, referenced, order, &mut count, scratch)?;
        if order[..count].iter().any(|&partition| {
            current
                .fkey_clone(identity, storage.table_position(partition))
                .is_none()
        }) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn clone_wanted(
    storage: &Storage,
    txid: u32,
    table: usize,
    key: &crate::storage::ForeignKey,
    clone: &crate::storage::ForeignKeyClone,
) -> bool {
    cloned_referenced_table(storage, txid, table, key).is_some_and(|referenced| {
        let partition = clone.partition.slot();
        partition < storage.table_count()
            && storage.relation_visible_to_current_session(partition, txid)
            && storage.table_position(partition) == clone.partition
            && partition != referenced
            && storage.partition_descends_from(partition, referenced, txid)
    })
}

/// The partitioned table a top-level foreign key of `table` references; a
/// partition's copy of its parent's key has no referenced-side clones.
fn cloned_referenced_table(
    storage: &Storage,
    txid: u32,
    table: usize,
    key: &crate::storage::ForeignKey,
) -> Option<usize> {
    if crate::sql::catalog::inherited_foreign_key_parent_oid(storage, txid, table, key) != 0 {
        return None;
    }
    let referenced = storage.find_visible(key.parent_schema.as_str(), key.parent.as_str(), txid)?;
    storage
        .table_def(referenced, txid)
        .partition
        .is_partitioned()
        .then_some(referenced)
}

/// `parent`'s partitions at every depth, each before its own partitions and
/// siblings in partition-descriptor order, as `addFkRecurseReferenced` visits
/// them.
pub(super) fn partitions_in_bound_order(
    storage: &Storage,
    txid: u32,
    parent: usize,
    out: &mut [usize],
    count: &mut usize,
    scratch: &mut [usize],
) -> Result<(), SqlError> {
    let mut found = 0usize;
    for child in 0..storage.table_count() {
        if storage.relation_visible_to_current_session(child, txid)
            && storage
                .table_def(child, txid)
                .partition
                .attachment
                .is_some_and(|attachment| usize::from(attachment.parent) == parent)
        {
            scratch[found] = child;
            found += 1;
        }
    }
    let (level, rest) = scratch.split_at_mut(found);
    let mut failed = None;
    level.sort_unstable_by(|&left, &right| {
        super::partition_bound_order(storage, left, right, txid).unwrap_or_else(|error| {
            failed.get_or_insert(error);
            core::cmp::Ordering::Equal
        })
    });
    if let Some(error) = failed {
        return Err(error);
    }
    for &child in level.iter() {
        out[*count] = child;
        *count += 1;
        if storage.table_def(child, txid).partition.is_partitioned() {
            partitions_in_bound_order(storage, txid, child, out, count, rest)?;
        }
    }
    Ok(())
}

/// PostgreSQL's `ChooseConstraintName(key, NULL, "", namespace)`: the key's
/// name with the first `_N` suffix no constraint in the namespace has.
pub(super) fn clone_name(
    storage: &Storage,
    definition: &TableDefBuilder,
    key: &str,
    table: usize,
    txid: u32,
) -> Result<SqlName, SqlError> {
    use core::fmt::Write as _;
    for number in 1..=u32::MAX {
        let mut label = crate::util::StackStr::<16>::new();
        write!(label, "{number}").expect("u32 clone suffix fits");
        let name = crate::storage::constraint_names::generated(key, None, label.as_str())?;
        if !storage.generated_name_taken(
            definition,
            name,
            txid,
            Some(table),
            crate::storage::GeneratedNameClass::Constraint,
        ) {
            return Ok(name);
        }
    }
    Err(sql_err!(
        sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "generated constraint names are exhausted"
    ))
}

/// Appends each partition (at any depth) of `table` that holds a copy of its
/// foreign key `oid`, with the copy's name, parents before their partitions.
pub(super) fn inherited_key_copies(
    storage: &Storage,
    txid: u32,
    table: usize,
    oid: i32,
    out: &mut [(usize, SqlName)],
    count: &mut usize,
) {
    for child in 0..storage.table_count() {
        if !storage.table_slot_visible_to(child, txid)
            || storage
                .table_def(child, txid)
                .partition
                .attachment
                .is_none_or(|attachment| usize::from(attachment.parent) != table)
        {
            continue;
        }
        let copy = storage
            .table_def(child, txid)
            .fkeys()
            .iter()
            .find(|key| {
                crate::sql::catalog::inherited_foreign_key_parent_oid(storage, txid, child, key)
                    == oid
            })
            .copied();
        if let Some(copy) = copy {
            out[*count] = (child, copy.name);
            *count += 1;
            inherited_key_copies(storage, txid, child, copy.oid(), out, count);
        }
    }
}
