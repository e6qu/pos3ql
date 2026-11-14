//! `ALTER COLUMN ... TYPE` rebuilds every object over a retyped column, as
//! PostgreSQL's `ATPostAlterTypeCleanup` does: it drops each key, exclusion
//! and explicit index, NOT NULL, CHECK and foreign key constraint and
//! extended statistics object that depends on the column and creates it again
//! under a new OID.
//!
//! The new OIDs follow PostgreSQL's passes. Each table of the statement's
//! work queue (the altered tables, then each table owning a foreign key that
//! references a retyped column) re-adds its key constraints and then its
//! plain indexes (`AT_PASS_OLD_INDEX`); then each re-adds its NOT NULL
//! constraints, newest discovered first, and its CHECK and foreign key
//! constraints in discovery order (`AT_PASS_OLD_CONSTR`); statistics follow
//! last. A partition's inherited copies are re-added by their parent's
//! constraint or index, right after it, so a partition index takes a newly
//! generated name. Discovery visits each retyped column's dependents in
//! `pg_depend` order, which is creation order.

use super::*;
use crate::sql::catalog::dependencies::{ObjectAddress, for_each_dependency};
use crate::storage::{ConstraintIdentity, RelationIdentity};

/// One constraint or index whose identity changes.
#[derive(Clone, Copy)]
enum Rekey {
    Constraint {
        table: usize,
        old: ConstraintIdentity,
        new: ConstraintIdentity,
    },
    KeyIndex {
        old: RelationIdentity,
        new: RelationIdentity,
    },
    /// An explicit index replaced by a new slot; foreign keys that depend on
    /// it follow.
    Index {
        old: RelationIdentity,
        new: RelationIdentity,
    },
    /// A partition's copy of a rebuilt key, which takes a newly generated
    /// name as `DefineIndex` gives one.
    RenameKey {
        table: usize,
        constraint: ConstraintIdentity,
    },
}

/// A constraint discovered over a retyped column.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Found {
    owner: usize,
    identity: ConstraintIdentity,
    kind: FoundKind,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FoundKind {
    /// Primary key, unique or exclusion constraint: re-added with its index.
    Key,
    NotNull,
    /// CHECK or foreign key.
    Other,
}

/// What `identity` names in `definition`, unless it is a copy its parent
/// re-adds (PostgreSQL skips constraints whose `conislocal` is false) or a
/// foreign key's partition clone, which its key re-adds.
fn classify(
    storage: &Storage,
    txid: u32,
    definition: &TableDef,
    identity: ConstraintIdentity,
) -> Option<FoundKind> {
    let parent = definition
        .partition
        .attachment
        .map(|attachment| storage.table_def(usize::from(attachment.parent), txid));
    if let Some(check) = definition
        .checks()
        .iter()
        .find(|check| check.identity == Some(identity))
    {
        let inherited = parent.as_ref().is_some_and(|parent| {
            parent
                .checks()
                .iter()
                .any(|candidate| candidate.name == check.name)
        });
        return (!inherited).then_some(FoundKind::Other);
    }
    if let Some(key) = definition
        .fkeys()
        .iter()
        .find(|key| key.identity == Some(identity))
    {
        let inherited = parent.as_ref().is_some_and(|parent| {
            parent
                .fkeys()
                .iter()
                .any(|candidate| candidate.name == key.name)
        });
        return (!inherited).then_some(FoundKind::Other);
    }
    for column in 0..definition.n_columns {
        let attribute = usize::from(definition.attribute_number(column) - 1);
        let metadata = &definition.columns()[column];
        if metadata.not_null.is_required()
            && definition.attribute_identities.as_ref()[attribute].not_null == Some(identity)
        {
            return metadata.not_null.is_local().then_some(FoundKind::NotNull);
        }
        if (metadata.primary || metadata.unique)
            && definition.column_constraint_identity(column) == Some(identity)
        {
            let inherited = parent.as_ref().is_some_and(|parent| {
                parent_key(parent, definition, KeyShape::Column(column)).is_some()
            });
            return (!inherited).then_some(FoundKind::Key);
        }
    }
    for (position, key) in definition.uniques().iter().enumerate() {
        if key.identity == Some(identity) {
            let inherited = parent.as_ref().is_some_and(|parent| {
                parent_key(parent, definition, KeyShape::Unique(position)).is_some()
            });
            return (!inherited).then_some(FoundKind::Key);
        }
    }
    for (position, key) in definition.exclusions().iter().enumerate() {
        if key.identity == Some(identity) {
            let inherited = parent.as_ref().is_some_and(|parent| {
                parent_key(parent, definition, KeyShape::Exclusion(position)).is_some()
            });
            return (!inherited).then_some(FoundKind::Key);
        }
    }
    None
}

/// A key or exclusion constraint of one table.
#[derive(Clone, Copy)]
enum KeyShape {
    Column(usize),
    Unique(usize),
    Exclusion(usize),
}

/// The key of `parent` that `child`'s key `shape` is the partition copy of:
/// the same kind over the same columns by name.
fn parent_key<P: crate::storage::DefinitionBacking, C: crate::storage::DefinitionBacking>(
    parent: &crate::storage::TableDefinition<P>,
    child: &crate::storage::TableDefinition<C>,
    shape: KeyShape,
) -> Option<KeyShape> {
    let same_column = |parent_column: usize, child_column: usize| {
        parent.columns()[parent_column].name == child.columns()[child_column].name
    };
    match shape {
        KeyShape::Column(column) => {
            let metadata = &child.columns()[column];
            (0..parent.n_columns)
                .find(|&candidate| {
                    same_column(candidate, column)
                        && parent.columns()[candidate].primary == metadata.primary
                        && parent.columns()[candidate].unique == metadata.unique
                })
                .map(KeyShape::Column)
        }
        KeyShape::Unique(position) => {
            let key = &child.uniques()[position];
            parent
                .uniques()
                .iter()
                .position(|candidate| {
                    candidate.kind == key.kind
                        && candidate.n_cols == key.n_cols
                        && candidate
                            .columns()
                            .iter()
                            .zip(key.columns())
                            .all(|(&left, &right)| {
                                same_column(usize::from(left), usize::from(right))
                            })
                })
                .map(KeyShape::Unique)
        }
        KeyShape::Exclusion(position) => {
            let exclusion = &child.exclusions()[position];
            let mapped = exclusion
                .map_columns(|column| {
                    parent
                        .column_index(child.columns()[usize::from(column)].name.as_str())
                        .map(|column| column as u16)
                        .ok_or_else(|| sql_err!(sqlstate::UNDEFINED_COLUMN, "column is absent"))
                })
                .ok()?;
            parent
                .exclusions()
                .iter()
                .position(|candidate| {
                    candidate.same_elements(&mapped) && candidate.predicate == exclusion.predicate
                })
                .map(KeyShape::Exclusion)
        }
    }
}

fn key_identities<B: crate::storage::DefinitionBacking>(
    definition: &crate::storage::TableDefinition<B>,
    shape: KeyShape,
) -> (Option<ConstraintIdentity>, Option<RelationIdentity>) {
    match shape {
        KeyShape::Column(column) => (
            definition.column_constraint_identity(column),
            definition.column_index_identity(column),
        ),
        KeyShape::Unique(position) => {
            let key = &definition.uniques()[position];
            (key.identity, key.index_identity)
        }
        KeyShape::Exclusion(position) => {
            let key = &definition.exclusions()[position];
            (key.identity, key.index_identity)
        }
    }
}

fn key_shape<B: crate::storage::DefinitionBacking>(
    definition: &crate::storage::TableDefinition<B>,
    identity: ConstraintIdentity,
) -> Option<KeyShape> {
    (0..definition.n_columns)
        .find(|&column| {
            let metadata = &definition.columns()[column];
            (metadata.primary || metadata.unique)
                && definition.column_constraint_identity(column) == Some(identity)
        })
        .map(KeyShape::Column)
        .or_else(|| {
            definition
                .uniques()
                .iter()
                .position(|key| key.identity == Some(identity))
                .map(KeyShape::Unique)
        })
        .or_else(|| {
            definition
                .exclusions()
                .iter()
                .position(|key| key.identity == Some(identity))
                .map(KeyShape::Exclusion)
        })
}

/// Partitions of `parent` at every depth, each before its own partitions,
/// siblings in partition bound order (`DefineIndex` and
/// `addFkRecurseReferencing`) or in table OID order
/// (`find_inheritance_children`, which CHECK and NOT NULL recursion uses).
fn descendants<'a>(
    storage: &Storage,
    txid: u32,
    parent: usize,
    bound_order: bool,
    arena: &'a Arena,
) -> Result<&'a [usize], SqlError> {
    let count = storage.table_count();
    let out = arena
        .alloc_slice_with(count, |_| usize::MAX)
        .map_err(|_| arena_full())?;
    let scratch = arena
        .alloc_slice_with(count, |_| usize::MAX)
        .map_err(|_| arena_full())?;
    let mut found = 0usize;
    if bound_order {
        foreign_key_clones::partitions_in_bound_order(
            storage, txid, parent, out, &mut found, scratch,
        )?;
        return Ok(&out[..found]);
    }
    // Depth first in table OID order: take the oldest unvisited child of the
    // table on top of the path, or step back up.
    let path = scratch;
    let mut depth = 0usize;
    path[0] = parent;
    loop {
        let current = path[depth];
        let next = (0..count)
            .filter(|&child| {
                !out[..found].contains(&child)
                    && storage.relation_visible_to_current_session(child, txid)
                    && storage
                        .table_def(child, txid)
                        .partition
                        .attachment
                        .is_some_and(|attachment| usize::from(attachment.parent) == current)
            })
            .min_by_key(|&child| storage.table(child).created_at());
        match next {
            Some(child) => {
                out[found] = child;
                found += 1;
                depth += 1;
                path[depth] = child;
            }
            None if depth == 0 => break,
            None => depth -= 1,
        }
    }
    Ok(&out[..found])
}

/// The key shape of `child` that is the partition copy of `parent`'s key
/// `shape`.
fn child_key<P: crate::storage::DefinitionBacking, C: crate::storage::DefinitionBacking>(
    parent: &crate::storage::TableDefinition<P>,
    shape: KeyShape,
    child: &crate::storage::TableDefinition<C>,
) -> Option<KeyShape> {
    let same = |left: KeyShape, right: KeyShape| match (left, right) {
        (KeyShape::Column(left), KeyShape::Column(right))
        | (KeyShape::Unique(left), KeyShape::Unique(right))
        | (KeyShape::Exclusion(left), KeyShape::Exclusion(right)) => left == right,
        _ => false,
    };
    (0..child.n_columns)
        .filter(|&column| child.columns()[column].primary || child.columns()[column].unique)
        .map(KeyShape::Column)
        .chain((0..child.uniques().len()).map(KeyShape::Unique))
        .chain((0..child.exclusions().len()).map(KeyShape::Exclusion))
        .find(|&candidate| {
            parent_key(parent, child, candidate).is_some_and(|matched| same(matched, shape))
        })
}

/// The constraint `oid` names, on the table that owns it, when PostgreSQL
/// re-adds it itself.
fn find_constraint(storage: &Storage, txid: u32, oid: i32) -> Option<Found> {
    (0..storage.table_count())
        .filter(|&table| storage.table_slot_visible_to(table, txid))
        .find_map(|table| {
            let definition = storage.table_def(table, txid);
            let identity = definition
                .constraint_identities()
                .flatten()
                .find(|identity| identity.oid() == oid)?;
            Some(
                classify(storage, txid, &definition, identity).map(|kind| Found {
                    owner: table,
                    identity,
                    kind,
                }),
            )
        })
        .flatten()
}

/// The key constraint whose index `oid` names, on `table`.
fn find_key_index(storage: &Storage, txid: u32, table: usize, oid: i32) -> Option<Found> {
    let definition = storage.table_def(table, txid);
    let shape = (0..definition.n_columns)
        .filter(|&column| {
            definition.columns()[column].primary || definition.columns()[column].unique
        })
        .map(KeyShape::Column)
        .chain((0..definition.uniques().len()).map(KeyShape::Unique))
        .chain((0..definition.exclusions().len()).map(KeyShape::Exclusion))
        .find(|&shape| {
            key_identities(&definition, shape)
                .1
                .is_some_and(|identity| identity.oid() == oid)
        })?;
    let identity = key_identities(&definition, shape).0?;
    classify(storage, txid, &definition, identity).map(|kind| Found {
        owner: table,
        identity,
        kind,
    })
}

/// Rebuilds what depends on the columns `actions` retype in `tables`, the
/// altered tables parent first.
pub(super) fn rebuild_retyped_dependents(
    storage: &mut Storage,
    wal: &mut Wal,
    txn: &mut TxnState,
    tables: &[usize],
    actions: &[AlterAction<'_>],
    arena: &Arena,
) -> Result<(), SqlError> {
    let txid = txn.txid;
    // Each retyped column, by table position and attribute number, in
    // subcommand order.
    let mut columns = ArenaList::new(arena);
    for (position, &table) in tables.iter().enumerate() {
        let definition = storage.table_def(table, txid);
        for action in actions {
            if let AlterAction::AlterColumnType { column, .. } = action
                && let Some(index) = definition.column_index(column)
            {
                columns
                    .push((position, i32::from(definition.attribute_number(index))))
                    .map_err(|_| arena_full())?;
            }
        }
    }
    if columns.is_empty() {
        return Ok(());
    }
    let columns = columns.as_slice();
    let table_oids = arena
        .alloc_slice_with(tables.len(), |position| {
            crate::sql::catalog::user_table_oid(storage, tables[position])
        })
        .map_err(|_| arena_full())?;
    let mut dependents = ArenaList::new(arena);
    for_each_dependency(storage, txid, arena, &mut |edge| {
        if edge.referenced.class != crate::sql::catalog::PG_CLASS_OID || edge.referenced.sub == 0 {
            return Ok(());
        }
        if let Some(ordinal) = columns.iter().position(|&(position, attribute)| {
            table_oids[position] == edge.referenced.object && attribute == edge.referenced.sub
        }) {
            dependents
                .push((ordinal, edge.dependent))
                .map_err(|_| arena_full())?;
        }
        Ok(())
    })?;
    // `pg_depend`'s scan of one column returns its dependents in creation
    // order.
    let dependents = dependents.as_mut_slice().map_err(|_| arena_full())?;
    dependents.sort_by_key(|&(ordinal, dependent): &(usize, ObjectAddress)| {
        (
            ordinal,
            crate::storage::OidBand::creation_order(dependent.object),
            dependent.class,
        )
    });

    let mut queue = ArenaList::new(arena);
    for &table in tables {
        queue.push(table).map_err(|_| arena_full())?;
    }
    let mut keys = ArenaList::<Found>::new(arena);
    let mut indexes = ArenaList::<(usize, usize)>::new(arena);
    let mut constraints = ArenaList::<Found>::new(arena);
    let mut statistics = ArenaList::<usize>::new(arena);
    for (position, &table) in tables.iter().enumerate() {
        // PostgreSQL's `changedConstraintOids` puts NOT NULL constraints
        // first, each in front of those found before it.
        let mut not_null = ArenaList::<Found>::new(arena);
        let mut changed = ArenaList::<Found>::new(arena);
        let remember = |found: Option<Found>,
                        not_null: &mut ArenaList<'_, Found>,
                        changed: &mut ArenaList<'_, Found>|
         -> Result<(), SqlError> {
            let Some(found) = found else {
                return Ok(());
            };
            if not_null.as_slice().contains(&found) || changed.as_slice().contains(&found) {
                return Ok(());
            }
            if found.kind == FoundKind::NotNull {
                not_null.push(found)
            } else {
                changed.push(found)
            }
            .map_err(|_| arena_full())
        };
        for &(ordinal, dependent) in dependents.iter() {
            if columns[ordinal].0 != position {
                continue;
            }
            match dependent.class {
                crate::sql::catalog::PG_CONSTRAINT_OID => remember(
                    find_constraint(storage, txid, dependent.object),
                    &mut not_null,
                    &mut changed,
                )?,
                crate::sql::catalog::PG_CLASS_OID => {
                    let explicit = (0..storage.index_count()).find(|&slot| {
                        storage
                            .index_visible_to(slot, txid)
                            .is_some_and(|index| index.relation_oid() == dependent.object)
                    });
                    match explicit {
                        // A partition's index is re-added by its parent's.
                        Some(slot) => {
                            let index = storage
                                .index_visible_to(slot, txid)
                                .expect("visible index found above");
                            if index.mutable_for(txid).parent.is_none()
                                && !indexes.as_slice().contains(&(table, slot))
                            {
                                indexes.push((table, slot)).map_err(|_| arena_full())?;
                            }
                        }
                        None => remember(
                            find_key_index(storage, txid, table, dependent.object),
                            &mut not_null,
                            &mut changed,
                        )?,
                    }
                }
                crate::sql::catalog::PG_STATISTIC_EXT_OID => {
                    if let Some((slot, _)) = storage
                        .extended_statistics_visible(txid)
                        .find(|(_, statistics)| statistics.oid() == dependent.object)
                        && !statistics.as_slice().contains(&slot)
                    {
                        statistics.push(slot).map_err(|_| arena_full())?;
                    }
                }
                _ => {}
            }
        }
        for found in not_null
            .as_slice()
            .iter()
            .rev()
            .chain(changed.as_slice().iter())
        {
            if found.kind == FoundKind::Key {
                keys.push(*found).map_err(|_| arena_full())?;
            } else {
                constraints.push(*found).map_err(|_| arena_full())?;
            }
            if !queue.as_slice().contains(&found.owner) {
                queue.push(found.owner).map_err(|_| arena_full())?;
            }
        }
    }

    // PostgreSQL drops every old index before re-adding any.
    let mut replaced = ArenaList::<(usize, crate::storage::IndexDef)>::new(arena);
    let mut comments = ArenaList::new(arena);
    for &(table, slot) in indexes.as_slice() {
        let index = storage
            .index_visible_to(slot, txid)
            .ok_or_else(|| sql_err!(sqlstate::UNDEFINED_OBJECT, "index does not exist"))?;
        let mut definition = index.definition_for(txid);
        definition.name = definition.name_for(txid);
        definition.mutable = definition.mutable_for(txid);
        definition.pending_definition = None;
        definition.pending_name = None;
        replaced
            .push((table, definition))
            .map_err(|_| arena_full())?;
        if let Some(comment) = storage.comment_text(
            crate::storage::CommentClass::Relation,
            definition.schema.as_str(),
            definition.name.as_str(),
            0,
            txid,
        ) {
            comments
                .push((definition.created_at, comment))
                .map_err(|_| arena_full())?;
        }
        for descendant in (0..storage.index_count()).rev() {
            if index_descends_from(storage, descendant, slot, txid) {
                stage_index_drop(storage, wal, txn, descendant)?;
            }
        }
        stage_index_drop(storage, wal, txn, slot)?;
    }

    let mut rekeys = ArenaList::<Rekey>::new(arena);
    // `AT_PASS_OLD_INDEX`: each table's keys, then its plain indexes.
    for &table in queue.as_slice() {
        for found in keys.as_slice().iter().filter(|found| found.owner == table) {
            let definition = storage.table_def(table, txid);
            let shape = key_shape(&definition, found.identity).ok_or_else(|| {
                sql_err!(sqlstate::INTERNAL_ERROR, "rebuilt key constraint is absent")
            })?;
            let rekey_key = |storage: &Storage,
                             rekeys: &mut ArenaList<'_, Rekey>,
                             table: usize,
                             shape: KeyShape|
             -> Result<(), SqlError> {
                let definition = storage.table_def(table, txid);
                let (Some(constraint), Some(index)) = key_identities(&definition, shape) else {
                    return Err(sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "rebuilt key has no identity"
                    ));
                };
                // `index_create`, then `index_constraint_create`.
                let new = storage.reserve_relation_identity()?;
                rekeys
                    .push(Rekey::KeyIndex { old: index, new })
                    .map_err(|_| arena_full())?;
                rekey_constraint(storage, rekeys, table, constraint)
            };
            rekey_key(storage, &mut rekeys, table, shape)?;
            if definition.partition.is_partitioned() {
                for &partition in descendants(storage, txid, table, true, arena)? {
                    // The parent's matching key, found through its own
                    // parent up to `table`.
                    let mut chain = [usize::MAX; 64];
                    let mut length = 0usize;
                    let mut cursor = partition;
                    while cursor != table && length < chain.len() {
                        chain[length] = cursor;
                        length += 1;
                        cursor = usize::from(
                            storage
                                .table_def(cursor, txid)
                                .partition
                                .attachment
                                .expect("a descendant has a partition parent")
                                .parent,
                        );
                    }
                    let mut matched = Some(shape);
                    let mut upper = table;
                    for &lower in chain[..length].iter().rev() {
                        matched = matched.and_then(|shape| {
                            child_key(
                                &storage.table_def(upper, txid),
                                shape,
                                &storage.table_def(lower, txid),
                            )
                        });
                        upper = lower;
                    }
                    if let Some(shape) = matched {
                        rekey_key(storage, &mut rekeys, partition, shape)?;
                        let Some(&Rekey::Constraint { new, .. }) = rekeys.as_slice().last() else {
                            unreachable!("a rebuilt key ends with its constraint");
                        };
                        rekeys
                            .push(Rekey::RenameKey {
                                table: partition,
                                constraint: new,
                            })
                            .map_err(|_| arena_full())?;
                    }
                }
            }
        }
        for &(_, definition) in replaced
            .as_slice()
            .iter()
            .filter(|(owner, _)| *owner == table)
        {
            // The rebuilt index keeps its comment (`idxcomment`).
            let comment = comments
                .as_slice()
                .iter()
                .find(|(created_at, _)| *created_at == definition.created_at)
                .map(|(_, comment)| *comment);
            let old = RelationIdentity::parse(definition.created_at).ok_or_else(|| {
                sql_err!(sqlstate::INTERNAL_ERROR, "rebuilt index has no identity")
            })?;
            let mut copy = definition;
            copy.created_at = 0;
            let slot = storage.create_index(copy, txid)?;
            finish_index_creation(storage, wal, txn, slot, table, arena)?;
            let new = storage
                .index_visible_to(slot, txid)
                .and_then(|index| RelationIdentity::parse(index.created_at))
                .ok_or_else(|| {
                    sql_err!(sqlstate::INTERNAL_ERROR, "rebuilt index has no identity")
                })?;
            rekeys
                .push(Rekey::Index { old, new })
                .map_err(|_| arena_full())?;
            if let Some(comment) = comment {
                stage_comment(
                    storage,
                    wal,
                    txn,
                    CommentIdentity {
                        class: crate::storage::CommentClass::Relation,
                        schema: definition.schema,
                        name: definition.name,
                        subid: 0,
                    },
                    Some(comment.as_str()),
                )?;
            }
            if storage.table_def(table, txid).partition.is_partitioned() {
                ensure_partition_index_children(
                    storage,
                    wal,
                    txn,
                    slot,
                    table,
                    PartitionIndexMatch::AnyEquivalent,
                    arena,
                )?;
            }
        }
    }
    // `AT_PASS_OLD_CONSTR`: each table's NOT NULL, CHECK and foreign key
    // constraints; a key's partition clones and a partitioned table's
    // inherited copies follow their constraint.
    for &table in queue.as_slice() {
        for found in constraints
            .as_slice()
            .iter()
            .filter(|found| found.owner == table)
        {
            let definition = storage.table_def(table, txid);
            enum Named {
                Check(SqlName),
                ForeignKey(SqlName),
                NotNull(SqlName),
            }
            let named = if let Some(check) = definition
                .checks()
                .iter()
                .find(|check| check.identity == Some(found.identity))
            {
                Named::Check(check.name)
            } else if let Some(key) = definition
                .fkeys()
                .iter()
                .find(|key| key.identity == Some(found.identity))
            {
                Named::ForeignKey(key.name)
            } else {
                let column = (0..definition.n_columns)
                    .find(|&column| {
                        definition.attribute_identities.as_ref()
                            [usize::from(definition.attribute_number(column) - 1)]
                        .not_null
                            == Some(found.identity)
                    })
                    .ok_or_else(|| {
                        sql_err!(sqlstate::INTERNAL_ERROR, "rebuilt constraint is absent")
                    })?;
                Named::NotNull(definition.columns()[column].name)
            };
            rekey_with_clones(storage, txid, &mut rekeys, table, found.identity)?;
            if definition.partition.is_partitioned() {
                let bound_order = matches!(named, Named::ForeignKey(_));
                for &partition in descendants(storage, txid, table, bound_order, arena)? {
                    let child = storage.table_def(partition, txid);
                    let identity = match named {
                        Named::Check(name) => child
                            .checks()
                            .iter()
                            .find(|check| check.name == name)
                            .and_then(|check| check.identity),
                        Named::ForeignKey(name) => child
                            .fkeys()
                            .iter()
                            .find(|key| key.name == name)
                            .and_then(|key| key.identity),
                        Named::NotNull(name) => {
                            child.column_index(name.as_str()).and_then(|column| {
                                child.columns()[column]
                                    .not_null
                                    .is_inherited()
                                    .then(|| {
                                        child.attribute_identities.as_ref()
                                            [usize::from(child.attribute_number(column) - 1)]
                                        .not_null
                                    })
                                    .flatten()
                            })
                        }
                    };
                    if let Some(identity) = identity {
                        rekey_with_clones(storage, txid, &mut rekeys, partition, identity)?;
                    }
                }
            }
        }
    }

    // Every table whose catalog row changes: the rebuilt objects' owners and
    // each table whose foreign key depends on a rebuilt index.
    let rekeys = rekeys.as_slice();
    for table in 0..storage.table_count() {
        if !storage.table_slot_visible_to(table, txid) {
            continue;
        }
        let (target, current) = storage.select_table_definition(table, txid);
        let mut next = TableDefBuilder::from_definition(&current);
        let mut changed = false;
        for identity in next.constraint_identities_mut() {
            for rekey in rekeys {
                if let Rekey::Constraint {
                    table: owner,
                    old,
                    new,
                } = *rekey
                    && owner == table
                    && *identity == Some(old)
                {
                    *identity = Some(new);
                    changed = true;
                    break;
                }
            }
        }
        let remap_index = |identity: RelationIdentity| {
            rekeys.iter().find_map(|rekey| match *rekey {
                Rekey::KeyIndex { old, new } | Rekey::Index { old, new } if old == identity => {
                    Some(new)
                }
                _ => None,
            })
        };
        for identity in next.implicit_index_identities_mut() {
            if let Some(new) = identity.and_then(remap_index) {
                *identity = Some(new);
                changed = true;
            }
        }
        let mut marks = next.key_index_marks;
        for mark in [&mut marks.clustered, &mut marks.replica_identity] {
            if let Some(new) = mark.and_then(remap_index) {
                *mark = Some(new);
                changed = true;
            }
        }
        next.key_index_marks = marks;
        for position in 0..next.n_fkey_clones {
            let foreign_key = next.fkey_clones[position].foreign_key;
            for rekey in rekeys {
                if let Rekey::Constraint {
                    table: owner,
                    old,
                    new,
                } = *rekey
                    && owner == table
                    && foreign_key == old
                {
                    next.fkey_clones[position].foreign_key = new;
                    changed = true;
                    break;
                }
            }
        }
        for position in 0..next.n_fkeys {
            if let Some(new) = next.fkeys[position]
                .referenced_index
                .identity()
                .and_then(remap_index)
            {
                next.fkeys[position].referenced_index = crate::storage::ReferencedIndex::Index(new);
                changed = true;
            }
        }
        for rekey in rekeys {
            if let Rekey::RenameKey {
                table: owner,
                constraint,
            } = *rekey
                && owner == table
            {
                rename_partition_key(storage, txid, table, &mut next, constraint, arena)?;
                changed = true;
            }
        }
        if changed {
            stage_table_definition(storage, wal, txn, target, current, next)?;
        }
    }

    // A key index's catalog slot, kept for its name or partition parent,
    // follows its key's new identity.
    let mut moved_slots = ArenaList::<(usize, usize)>::new(arena);
    for rekey in rekeys {
        let Rekey::KeyIndex { old, new } = *rekey else {
            continue;
        };
        let Some(slot) = (0..storage.index_count()).find(|&slot| {
            storage
                .index_visible_to(slot, txid)
                .is_some_and(|index| index.created_at == old.created_at())
        }) else {
            continue;
        };
        let index = storage
            .index_visible_to(slot, txid)
            .expect("visible slot found above");
        let table = storage
            .index_table_slot_to(slot, txid)
            .ok_or_else(|| sql_err!(sqlstate::INTERNAL_ERROR, "key index has no table"))?;
        let mut definition = index.definition_for(txid);
        definition.mutable = definition.mutable_for(txid);
        definition.pending_definition = None;
        definition.pending_name = None;
        definition.created_at = new.created_at();
        let owner = storage.table_def(table, txid);
        definition.name = (0..owner.n_columns)
            .filter(|&column| owner.columns()[column].primary || owner.columns()[column].unique)
            .map(KeyShape::Column)
            .chain((0..owner.uniques().len()).map(KeyShape::Unique))
            .chain((0..owner.exclusions().len()).map(KeyShape::Exclusion))
            .find(|&shape| key_identities(&owner, shape).1 == Some(new))
            .map(|shape| key_name(&owner, shape))
            .ok_or_else(|| sql_err!(sqlstate::INTERNAL_ERROR, "rebuilt key is absent"))?;
        definition.mutable.parent = definition
            .mutable
            .parent
            .map(|parent| {
                moved_slots
                    .as_slice()
                    .iter()
                    .find(|(old, _)| *old == usize::from(parent))
                    .map_or(Ok(parent), |&(_, new)| {
                        u16::try_from(new).map_err(|_| {
                            sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "too many indexes")
                        })
                    })
            })
            .transpose()?;
        drop(owner);
        stage_index_drop(storage, wal, txn, slot)?;
        let replacement = register_reindex_definition(storage, wal, txn, table, definition)?;
        moved_slots
            .push((slot, replacement))
            .map_err(|_| arena_full())?;
    }

    // `AT_PASS_MISC`: extended statistics.
    for &slot in statistics.as_slice() {
        let old = storage.extended_statistics(slot);
        let mutable = old.definition_for(txid);
        let keys = old.keys_for(txid);
        let mut spec = crate::storage::ExtendedStatisticsSpec {
            creation: crate::storage::CatalogCreation::New,
            schema: mutable.schema,
            name: mutable.name,
            table: old.table,
            target: mutable.target,
            keys: [crate::storage::ExtendedStatisticsKey::Column(SqlName::EMPTY);
                crate::storage::MAX_EXTENDED_STATISTICS_KEYS],
            n_keys: keys.len() as u8,
            kinds: old.kinds,
            expression_only: old.expression_only,
        };
        spec.keys[..keys.len()].copy_from_slice(keys);
        let temporary = storage.table_def(usize::from(old.table), txid).persistence
            == crate::storage::RelationPersistence::Temporary;
        storage.drop_extended_statistics(slot, txid)?;
        if !temporary {
            let lsn = storage.bump_lsn();
            if let Err(error) = wal.stage_reserved(
                txid,
                lsn,
                &crate::wal::WalOp::DropExtendedStatistics {
                    schema: mutable.schema.as_str(),
                    name: mutable.name.as_str(),
                },
            ) {
                storage.rollback_extended_statistics_drop(slot, txid);
                return Err(error);
            }
        }
        if let Err(error) = txn.record_ddl(
            storage,
            crate::sql::txn::DdlUndo::StatisticsDropped(slot as u32),
        ) {
            storage.rollback_extended_statistics_drop(slot, txid);
            return Err(error);
        }
        let created = storage.create_extended_statistics(spec, txid)?;
        if let Err(error) = stage_extended_statistics_definition(storage, wal, txn, created) {
            storage.rollback_extended_statistics_create(created);
            return Err(error);
        }
        if let Err(error) = txn.record_ddl(
            storage,
            crate::sql::txn::DdlUndo::StatisticsCreated(created as u32),
        ) {
            storage.rollback_extended_statistics_create(created);
            return Err(error);
        }
    }
    Ok(())
}

fn key_name<B: crate::storage::DefinitionBacking>(
    definition: &crate::storage::TableDefinition<B>,
    shape: KeyShape,
) -> SqlName {
    match shape {
        KeyShape::Column(column) => definition
            .column_key_name(column)
            .expect("an admitted inline key is named"),
        KeyShape::Unique(position) => definition.uniques()[position].name,
        KeyShape::Exclusion(position) => definition.exclusions()[position].name,
    }
}

/// Names the partition key `constraint` of `definition` as `DefineIndex`
/// names a partition's copy of its parent's index: generated from the table
/// and key columns, its own former name no longer taken.
fn rename_partition_key(
    storage: &Storage,
    txid: u32,
    table: usize,
    definition: &mut TableDefBuilder,
    constraint: ConstraintIdentity,
    arena: &Arena,
) -> Result<(), SqlError> {
    let shape = key_shape(definition, constraint)
        .ok_or_else(|| sql_err!(sqlstate::INTERNAL_ERROR, "renamed key is absent"))?;
    let class = crate::storage::GeneratedNameClass::IndexConstraint;
    match shape {
        KeyShape::Column(column) => {
            definition.take_column_key_name(column);
            let primary = definition.columns[column].primary;
            let columns = [column as u16];
            let name = ddl::auto_key_name(
                storage,
                definition,
                if primary { &[] } else { &columns },
                if primary { "pkey" } else { "key" },
                txid,
                Some(table),
                class,
            )?;
            definition.set_column_key_name(column, name)
        }
        KeyShape::Unique(position) => {
            definition.uniques[position].name = SqlName::EMPTY;
            let key = definition.uniques[position];
            let name = ddl::auto_key_name(
                storage,
                definition,
                if key.kind.is_primary() {
                    &[]
                } else {
                    key.columns()
                },
                if key.kind.is_primary() { "pkey" } else { "key" },
                txid,
                Some(table),
                class,
            )?;
            definition.uniques[position].name = name;
            Ok(())
        }
        KeyShape::Exclusion(position) => {
            definition.exclusions[position].name = SqlName::EMPTY;
            let exclusion = definition.exclusions[position];
            let name = ddl::auto_exclusion_name(
                storage,
                definition,
                &exclusion,
                txid,
                Some(table),
                arena,
            )?;
            definition.exclusions[position].name = name;
            Ok(())
        }
    }
}

/// Gives the constraint `old` of `table` a new identity.
fn rekey_constraint(
    storage: &Storage,
    rekeys: &mut ArenaList<'_, Rekey>,
    table: usize,
    old: ConstraintIdentity,
) -> Result<(), SqlError> {
    let new = storage.reserve_constraint_identity()?;
    rekeys
        .push(Rekey::Constraint { table, old, new })
        .map_err(|_| arena_full())
}

/// Gives the constraint `identity` of `table` a new identity, then each of
/// its partition clones when it is a foreign key, as `addFkConstraint`
/// re-creates them.
fn rekey_with_clones(
    storage: &Storage,
    txid: u32,
    rekeys: &mut ArenaList<'_, Rekey>,
    table: usize,
    identity: ConstraintIdentity,
) -> Result<(), SqlError> {
    rekey_constraint(storage, rekeys, table, identity)?;
    let definition = storage.table_def(table, txid);
    for clone in definition
        .fkey_clones()
        .iter()
        .filter(|clone| clone.foreign_key == identity)
    {
        if let Some(clone) = clone.identity {
            rekey_constraint(storage, rekeys, table, clone)?;
        }
    }
    Ok(())
}
