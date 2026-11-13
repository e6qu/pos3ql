//! Statement images of base catalog relations.
//!
//! A correlated subquery over a catalog relation (`EXISTS (SELECT 1 FROM
//! pg_type el WHERE el.oid = t.typelem)`, as psql's `\dT` issues) runs once
//! per outer row. PostgreSQL reads every scan of a statement under one
//! snapshot, so each run may reuse one image of the relation instead of
//! rebuilding it. Images live in the statement arena's persistent tail, which
//! per-row rewinds cannot reclaim, and are reused only while the arena,
//! transaction, role, database, catalog edit sequence and statement effect
//! epoch are unchanged.
//!
//! A statement running under a catalog snapshot (see [`enter_statement`])
//! keys its images to that snapshot instead: they are built when the
//! statement starts and pinned until it ends, so a routine that changes the
//! catalog in the middle of the statement leaves the statement's own scans
//! reading the catalog as it began. A volatile routine's statements run
//! under a fresh snapshot of their own ([`enter_fresh`]).

use core::cell::RefCell;

use crate::mem::arena::{Arena, ArenaHandle};
use crate::sql::ast::Collation;
use crate::sql::catalog_snapshot::CatalogScan;
use crate::sql::eval::{SqlError, arena_full};
use crate::storage::{DatabaseOid, Storage};

const CAPACITY: usize = 16;
/// Images pinned by running statement snapshots, nested calls included.
const PINNED_CAPACITY: usize = 256;
/// Statement snapshots nested inside one another.
const MAX_SNAPSHOT_DEPTH: usize = 64;

/// What a catalog image was read under.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ImageSnapshot {
    /// Valid while the catalog and the statement's effects are unchanged.
    Live { catalog_edits: u64, effects: u64 },
    /// Fixed for the statement snapshot with this identity.
    Statement(u64),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct ImageKey {
    relation: usize,
    sequential: bool,
    txid: u32,
    role: Option<usize>,
    database: DatabaseOid,
    snapshot: ImageSnapshot,
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
            snapshot: current_snapshot().map_or_else(
                || ImageSnapshot::Live {
                    catalog_edits: storage.catalog_edit_sequence(),
                    effects: super::statement_effect_epoch(),
                },
                ImageSnapshot::Statement,
            ),
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
    /// The arena whose persistent tail holds the rows. A statement snapshot's
    /// images may live in the statement arena while its scans run in an
    /// execution arena; the handle stops answering once that owner resets.
    owner: *const Arena,
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
    pinned: [Option<Image>; PINNED_CAPACITY],
    hashes: [Option<KeyHashes>; CAPACITY],
    next_hashes: usize,
}

std::thread_local! {
    static IMAGES: RefCell<Images> = const {
        RefCell::new(Images {
            entries: [None; CAPACITY],
            next: 0,
            pinned: [None; PINNED_CAPACITY],
            hashes: [None; CAPACITY],
            next_hashes: 0,
        })
    };
}

/// The image retained under `key` by the statement arena that owns `arena`.
pub(super) fn cached<'a>(arena: &'a Arena, key: ImageKey) -> Option<&'a [&'a [u8]]> {
    let arena = arena.retention_owner();
    IMAGES.with(|images| {
        let images = images.borrow();
        let live = images
            .entries
            .iter()
            .flatten()
            .filter(|image| image.key == key && core::ptr::eq(image.owner, arena))
            .find_map(|image| arena.retained(image.rows));
        live.or_else(|| {
            images
                .pinned
                .iter()
                .flatten()
                .filter(|image| image.key == key)
                // SAFETY: a pinned image is dropped when its statement
                // snapshot ends, before its owner arena can be reset or
                // freed, so the owner is alive here.
                .find_map(|image| unsafe { &*image.owner }.retained(image.rows))
        })
        .map(|(rows, len)| {
            // SAFETY: a handle is only retained by the owning arena while
            // its identity is unchanged, and `retain` copied every row and
            // the row list into that arena's durable tail. They stay valid
            // until the owner's `Arena::reset`, which the statement that
            // reads them outlives.
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
    let arena = image_owner(arena, key);
    let full = || {
        if matches!(key.snapshot, ImageSnapshot::Statement(_)) {
            crate::sql_err!(
                crate::sql::eval::sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "statement catalog images exceed catalog_image_bytes"
            )
        } else {
            arena_full()
        }
    };
    let retained = arena
        .alloc_persistent_slice_with(rows.len(), |_| &[] as &'a [u8])
        .map_err(|_| full())?;
    for (output, row) in retained.iter_mut().zip(rows) {
        *output = arena
            .alloc_persistent_slice_with(row.len(), |index| row[index])
            .map_err(|_| full())?;
    }
    let handle = arena
        .retain((retained.as_ptr().cast::<&'static [u8]>(), retained.len()))
        .map_err(|_| full())?;
    record(arena, key, handle)?;
    Ok(retained)
}

/// Records `rows`, already retained for `key`'s owner, under a second `key`.
pub(super) fn share<'a>(
    arena: &'a Arena,
    key: ImageKey,
    rows: &'a [&'a [u8]],
) -> Result<(), SqlError> {
    let arena = image_owner(arena, key);
    let handle = arena
        .retain((rows.as_ptr().cast::<&'static [u8]>(), rows.len()))
        .map_err(|_| arena_full())?;
    record(arena, key, handle)
}

fn record(arena: &Arena, key: ImageKey, handle: RowsHandle) -> Result<(), SqlError> {
    let image = Image {
        key,
        rows: handle,
        owner: arena,
    };
    IMAGES.with(|images| {
        let mut images = images.borrow_mut();
        if matches!(key.snapshot, ImageSnapshot::Statement(_)) {
            let slot = images
                .pinned
                .iter()
                .position(Option::is_none)
                .ok_or_else(pinned_images_full)?;
            images.pinned[slot] = Some(image);
            return Ok(());
        }
        let slot = images.next;
        images.entries[slot] = Some(image);
        images.next = (slot + 1) % CAPACITY;
        Ok(())
    })
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

fn pinned_images_full() -> SqlError {
    crate::sql_err!(
        crate::sql::eval::sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "statement catalog snapshots read more than {} catalog relations",
        PINNED_CAPACITY
    )
}

/// The arena an image under `key` lives in: a statement snapshot's images
/// live in the workspace's catalog image arena, which nested statements'
/// execution arena resets leave intact; others in the reading arena.
fn image_owner(arena: &Arena, key: ImageKey) -> &Arena {
    if matches!(key.snapshot, ImageSnapshot::Statement(_))
        && let Some(images) = SNAPSHOTS.with(|snapshots| snapshots.borrow().arena)
    {
        // SAFETY: the catalog image arena belongs to the query workspace,
        // which outlives every statement snapshot taken over it.
        return unsafe { &*images };
    }
    arena.retention_owner()
}

struct Snapshots {
    /// `Some(identity)` for a statement snapshot, `None` for a volatile
    /// routine's fresh reads.
    frames: [Option<u64>; MAX_SNAPSHOT_DEPTH],
    /// Whether the frame's statement built its images when it started; a
    /// statement that runs no user routine reads live images instead.
    pinned: [bool; MAX_SNAPSHOT_DEPTH],
    depth: usize,
    next: u64,
    /// The selected workspace's catalog image arena.
    arena: Option<*const Arena>,
}

std::thread_local! {
    static SNAPSHOTS: RefCell<Snapshots> = const {
        RefCell::new(Snapshots {
            frames: [None; MAX_SNAPSHOT_DEPTH],
            pinned: [false; MAX_SNAPSHOT_DEPTH],
            depth: 0,
            next: 1,
            arena: None,
        })
    };
}

fn current_snapshot() -> Option<u64> {
    SNAPSHOTS.with(|snapshots| {
        let snapshots = snapshots.borrow();
        snapshots
            .depth
            .checked_sub(1)
            .filter(|top| snapshots.pinned[*top])
            .and_then(|top| snapshots.frames[top])
    })
}

/// Pins the running statement's catalog images: built now, before it runs
/// a routine that could change the catalog, they stay as they are.
pub(super) fn pin_statement_images() {
    SNAPSHOTS.with(|snapshots| {
        let mut snapshots = snapshots.borrow_mut();
        if let Some(top) = snapshots.depth.checked_sub(1)
            && snapshots.frames[top].is_some()
        {
            snapshots.pinned[top] = true;
        }
    });
}

/// Ends a catalog snapshot and unpins the images read under it.
#[must_use = "the snapshot lasts while its scope is alive"]
pub(crate) struct CatalogSnapshotScope {
    depth: usize,
}

impl Drop for CatalogSnapshotScope {
    fn drop(&mut self) {
        let ended = SNAPSHOTS.with(|snapshots| {
            let mut snapshots = snapshots.borrow_mut();
            let ended = snapshots.frames[self.depth];
            snapshots.depth = self.depth;
            ended
        });
        if let Some(identity) = ended {
            IMAGES.with(|images| {
                for image in images.borrow_mut().pinned.iter_mut() {
                    if image.is_some_and(|image| {
                        image.key.snapshot == ImageSnapshot::Statement(identity)
                    }) {
                        *image = None;
                    }
                }
            });
        }
    }
}

fn push(frame: impl FnOnce(&mut u64) -> Option<u64>) -> Result<CatalogSnapshotScope, SqlError> {
    SNAPSHOTS.with(|snapshots| {
        let mut snapshots = snapshots.borrow_mut();
        let depth = snapshots.depth;
        if depth == MAX_SNAPSHOT_DEPTH {
            return Err(crate::sql_err!(
                crate::sql::eval::sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "statement catalog snapshots nest more than {} deep",
                MAX_SNAPSHOT_DEPTH
            ));
        }
        let pushed = frame(&mut snapshots.next);
        snapshots.frames[depth] = pushed;
        snapshots.pinned[depth] = false;
        snapshots.depth = depth + 1;
        Ok(CatalogSnapshotScope { depth })
    })
}

/// Starts a statement's catalog snapshot: its catalog images, kept in
/// `images` (the workspace's catalog image arena), stay fixed until the scope
/// ends.
pub(crate) fn enter_statement(images: &Arena) -> Result<CatalogSnapshotScope, SqlError> {
    SNAPSHOTS.with(|snapshots| snapshots.borrow_mut().arena = Some(images));
    push(|next| {
        let identity = *next;
        *next = next.wrapping_add(1);
        Some(identity)
    })
}

/// A volatile routine's statements read the catalog as it is now.
pub(crate) fn enter_fresh() -> Result<CatalogSnapshotScope, SqlError> {
    push(|_| None)
}
