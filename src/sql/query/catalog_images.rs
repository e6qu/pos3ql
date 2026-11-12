//! Statement images of base catalog relations.
//!
//! A correlated subquery over a catalog relation (`EXISTS (SELECT 1 FROM
//! pg_type el WHERE el.oid = t.typelem)`, as psql's `\dT` issues) runs once
//! per outer row. PostgreSQL reads every scan of a statement under one
//! snapshot, so each run may reuse one image of the relation instead of
//! rebuilding it. Images live in the statement arena's persistent tail, which
//! per-row rewinds cannot reclaim, and are reused only while the arena,
//! transaction, role, database, catalog edit sequence and statement effect
//! epoch are unchanged: a routine that writes or changes the catalog in the
//! middle of a statement still sees its own change on its next read.

use core::cell::RefCell;

use crate::mem::arena::{Arena, ArenaHandle};
use crate::sql::ast::Collation;
use crate::sql::catalog_snapshot::CatalogScan;
use crate::sql::eval::{SqlError, arena_full};
use crate::storage::{DatabaseOid, Storage};

const CAPACITY: usize = 16;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct ImageKey {
    relation: usize,
    sequential: bool,
    txid: u32,
    role: Option<usize>,
    database: DatabaseOid,
    catalog_edits: u64,
    effects: u64,
}

impl ImageKey {
    /// The key of a `pg_catalog` base relation's image; views, information
    /// schema relations and relations computed from session state have none.
    pub(super) fn new(
        storage: &Storage,
        namespace: &str,
        table: &str,
        scan: CatalogScan,
        txid: u32,
    ) -> Option<Self> {
        if namespace != "pg_catalog" {
            return None;
        }
        let relation = crate::sql::catalog_snapshot_keys::RELATIONS
            .iter()
            .position(|relation| relation.name == table)?;
        Some(Self {
            relation,
            sequential: matches!(scan, CatalogScan::Sequential),
            txid,
            role: storage.current_role_slot(txid),
            database: storage.current_database_oid(),
            catalog_edits: storage.catalog_edit_sequence(),
            effects: super::statement_effect_epoch(),
        })
    }
}

#[cfg(test)]
std::thread_local! {
    /// Images built on this thread, for tests that bound catalog rebuilds.
    pub(crate) static IMAGE_BUILDS: core::cell::Cell<u64> = const { core::cell::Cell::new(0) };
}

type RowsHandle = ArenaHandle<(*const &'static [u8], usize)>;

#[derive(Clone, Copy)]
struct Image {
    key: ImageKey,
    rows: RowsHandle,
}

type HashesHandle = ArenaHandle<(*const (u64, u32), usize)>;

/// Sorted key hashes of one image column, for keyed scans.
#[derive(Clone, Copy)]
struct KeyHashes {
    rows: *const &'static [u8],
    column: usize,
    collation: Collation,
    hashes: HashesHandle,
}

struct Images {
    entries: [Option<Image>; CAPACITY],
    next: usize,
    hashes: [Option<KeyHashes>; CAPACITY],
    next_hashes: usize,
}

std::thread_local! {
    static IMAGES: RefCell<Images> = const {
        RefCell::new(Images {
            entries: [None; CAPACITY],
            next: 0,
            hashes: [None; CAPACITY],
            next_hashes: 0,
        })
    };
}

/// The image retained under `key` by the statement arena that owns `arena`.
pub(super) fn cached<'a>(arena: &'a Arena, key: ImageKey) -> Option<&'a [&'a [u8]]> {
    let arena = arena.retention_owner();
    IMAGES.with(|images| {
        images
            .borrow()
            .entries
            .iter()
            .flatten()
            .filter(|image| image.key == key)
            .find_map(|image| arena.retained(image.rows))
            .map(|(rows, len)| {
                // SAFETY: a handle is only retained by the owning arena while
                // its identity is unchanged, and `retain` copied every row and
                // the row list into that arena's durable tail. They stay valid
                // until `Arena::reset`, which needs `&mut` and therefore ends
                // every borrow of the owner and its regions, including `'a`.
                unsafe { core::slice::from_raw_parts(rows.cast::<&'a [u8]>(), len) }
            })
    })
}

/// Copies `rows` into the persistent tail of the statement arena that owns
/// `arena` and records them under `key`. Correlated subqueries run in scoped
/// child regions, so the owner's tail is what outlives each run.
pub(super) fn retain<'a>(
    arena: &'a Arena,
    key: ImageKey,
    rows: &[&[u8]],
) -> Result<&'a [&'a [u8]], SqlError> {
    #[cfg(test)]
    IMAGE_BUILDS.with(|count| count.set(count.get() + 1));
    let arena = arena.retention_owner();
    let retained = arena
        .alloc_persistent_slice_with(rows.len(), |_| &[] as &'a [u8])
        .map_err(|_| arena_full())?;
    for (output, row) in retained.iter_mut().zip(rows) {
        *output = arena
            .alloc_persistent_slice_with(row.len(), |index| row[index])
            .map_err(|_| arena_full())?;
    }
    let handle = arena
        .retain((retained.as_ptr().cast::<&'static [u8]>(), retained.len()))
        .map_err(|_| arena_full())?;
    IMAGES.with(|images| {
        let mut images = images.borrow_mut();
        let slot = images.next;
        images.entries[slot] = Some(Image { key, rows: handle });
        images.next = (slot + 1) % CAPACITY;
    });
    Ok(retained)
}

/// The sorted `(hash, row position)` keys of `rows`' `column`. A retained
/// image keeps them beside its rows for the rest of the statement, so each
/// run of a correlated subquery probes instead of rehashing; other sources
/// are hashed by `build` for this scan only.
pub(super) fn key_hashes<'a>(
    arena: &'a Arena,
    rows: &'a [&'a [u8]],
    column: usize,
    collation: Collation,
    build: impl FnOnce(&'a Arena) -> Result<&'a [(u64, u32)], SqlError>,
) -> Result<&'a [(u64, u32)], SqlError> {
    let owner = arena.retention_owner();
    let rows_pointer = rows.as_ptr().cast::<&'static [u8]>();
    let retained = IMAGES.with(|images| {
        images
            .borrow()
            .entries
            .iter()
            .flatten()
            .filter_map(|image| owner.retained(image.rows))
            .any(|retained| retained == (rows_pointer, rows.len()))
    });
    if !retained {
        return build(arena);
    }
    let cached = IMAGES.with(|images| {
        images
            .borrow()
            .hashes
            .iter()
            .flatten()
            .filter(|hashes| {
                (hashes.rows, hashes.column, hashes.collation) == (rows_pointer, column, collation)
            })
            .find_map(|hashes| owner.retained(hashes.hashes))
    });
    if let Some((hashes, len)) = cached {
        // SAFETY: as for `cached`: the owner's matching identity proves these
        // durable-tail hashes are still allocated and unchanged.
        return Ok(unsafe { core::slice::from_raw_parts(hashes, len) });
    }
    let built = build(arena)?;
    let durable = owner
        .alloc_persistent_slice_with(built.len(), |index| built[index])
        .map_err(|_| arena_full())?;
    let handle = owner
        .retain((durable.as_ptr(), durable.len()))
        .map_err(|_| arena_full())?;
    IMAGES.with(|images| {
        let mut images = images.borrow_mut();
        let slot = images.next_hashes;
        images.hashes[slot] = Some(KeyHashes {
            rows: rows_pointer,
            column,
            collation,
            hashes: handle,
        });
        images.next_hashes = (slot + 1) % CAPACITY;
    });
    Ok(durable)
}
