//! PostgreSQL's dependency report for `DROP`: one ordered walk of the
//! catalog's dependency edges (`findDependentObjects`) and one report of what
//! it reached (`reportDependentObjects`).
//!
//! The walk visits an object's dependents newest first (by creation stamp,
//! then catalog and sub-object number), adds each dependent before the object
//! it depends on, and the report lists the walk in reverse. That reproduces
//! PostgreSQL's order: a row type's users come first, every object is followed
//! by its own dependents, and columns of one table appear last to first. An
//! object reached through an automatic, internal, partition or extension
//! dependency is dropped silently; every other one makes `RESTRICT` fail with
//! one `DETAIL` list and `CASCADE` emit one notice.

use core::fmt::Write as _;

use crate::mem::arena::Arena;
use crate::sql::catalog::dependencies::{DependencyEdge, ObjectAddress, for_each_dependency};
use crate::sql::eval::sqlstate;
use crate::sql::eval::{MAX_DIAGNOSTIC_DETAIL_BYTES, SqlError};
use crate::storage::Storage;
use crate::util::StackStr;
use crate::{sql_err, stack_format};

const NORMAL: u16 = 1 << 0;
const AUTO: u16 = 1 << 1;
const INTERNAL: u16 = 1 << 2;
const PARTITION: u16 = 1 << 3;
const EXTENSION: u16 = 1 << 4;
const REVERSE: u16 = 1 << 5;
const ORIGINAL: u16 = 1 << 6;
const SUBOBJECT: u16 = 1 << 7;
/// The object has partition dependencies; one of them must be dropped too.
const IS_PART: u16 = 1 << 8;
/// Silent: reached through a dependency that never needs `CASCADE`.
const SILENT: u16 = AUTO | INTERNAL | PARTITION | EXTENSION;
/// PostgreSQL's `MAX_REPORTED_DEPS`.
const MAX_REPORTED: usize = 100;

#[derive(Clone, Copy)]
struct Target {
    object: ObjectAddress,
    flags: u16,
    /// The object whose dependent this one is, for `depends on`, or its
    /// partition owner when it has one.
    dependee: Option<ObjectAddress>,
}

#[derive(Clone, Copy)]
struct Frame {
    object: ObjectAddress,
    flags: u16,
    /// The partition owner, when the object has partition dependencies.
    partition_owner: Option<ObjectAddress>,
    /// Sorted dependents in `pending[first..first + count]`.
    first: usize,
    count: usize,
    next: usize,
}

/// Everything one `DROP` reaches, in deletion order (dependents first).
pub(crate) struct DependencyWalk<'a> {
    targets: &'a [Target],
}

/// The position of an object in PostgreSQL's dependent order. pos3ql's OIDs
/// sit in per-family bands, but every band's ordinal is the catalog creation
/// stamp, which orders objects as PostgreSQL's single OID counter does.
fn creation_order(object: ObjectAddress) -> u64 {
    crate::storage::OidBand::creation_order(object.object)
}

/// When `object` was created, with a rank breaking ties. A TOAST table's and
/// its index's OIDs follow identities drawn at other times, but PostgreSQL
/// creates them after the table's columns and constraints and before its
/// key indexes, so they order just before the oldest key constraint or index.
fn creation_key(storage: &Storage, txid: u32, object: ObjectAddress) -> (u64, u8) {
    let toast_rank = if crate::storage::OidBand::ToastRelation.contains(object.object) {
        Some(0)
    } else if crate::storage::OidBand::of(object.object)
        == crate::storage::OidBand::of(crate::sql::catalog::FIRST_TOAST_INDEX_OID)
    {
        Some(1)
    } else {
        None
    };
    if let Some(rank) = toast_rank {
        let owner = (0..storage.table_count()).find(|&slot| {
            storage.table_slot_visible_to(slot, txid) && {
                let table = storage.table_def(slot, txid);
                table.has_toast
                    && if rank == 0 {
                        crate::sql::catalog::toast_relation_oid(storage, slot) == object.object
                    } else {
                        crate::sql::catalog::toast_index_oid(storage, slot, txid) == object.object
                    }
            }
        });
        if let Some(slot) = owner {
            let table = storage.table_def(slot, txid);
            let keys = (0..table.columns().len())
                .filter_map(|column| table.column_constraint_identity(column))
                .map(|identity| creation_order(ObjectAddress::new(0, identity.oid(), 0)))
                .chain(
                    table
                        .implicit_index_identities()
                        .flatten()
                        .map(|identity| creation_order(ObjectAddress::new(0, identity.oid(), 0))),
                )
                .min();
            if let Some(boundary) = keys {
                return (boundary, rank);
            }
        }
    }
    (creation_order(object), 2)
}

/// PostgreSQL's `object_address_comparator`: newest first, then catalog, then
/// sub-object number.
fn visit_order(
    storage: &Storage,
    txid: u32,
    left: &ObjectAddress,
    right: &ObjectAddress,
) -> core::cmp::Ordering {
    creation_key(storage, txid, *right)
        .cmp(&creation_key(storage, txid, *left))
        .then(left.class.cmp(&right.class))
        .then((left.sub as u32).cmp(&(right.sub as u32)))
}

/// PostgreSQL's `IsPinnedObject`: initdb's objects below
/// `FirstUnpinnedObjectId` (12000), except large objects, databases and the
/// `public` schema.
fn pinned(object: ObjectAddress) -> bool {
    const FIRST_UNPINNED_OBJECT_ID: i32 = 12_000;
    const LARGE_OBJECT_RELATION_ID: i32 = 2613;
    const DATABASE_RELATION_ID: i32 = 1262;
    const NAMESPACE_RELATION_ID: i32 = 2615;
    const PUBLIC_NAMESPACE: i32 = 2200;
    (0..FIRST_UNPINNED_OBJECT_ID).contains(&object.object)
        && object.class != DATABASE_RELATION_ID
        && object.class != LARGE_OBJECT_RELATION_ID
        && !(object.class == NAMESPACE_RELATION_ID && object.object == PUBLIC_NAMESPACE)
}

fn same_object(left: ObjectAddress, right: ObjectAddress) -> bool {
    left.class == right.class && left.object == right.object
}

fn table_full() -> SqlError {
    sql_err!(
        sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "dependency report exceeds the statement arena (sql_arena_bytes)"
    )
}

/// Walks every object `originals` take with them.
pub(crate) fn walk<'a>(
    storage: &Storage,
    txid: u32,
    originals: &[ObjectAddress],
    arena: &'a Arena,
) -> Result<DependencyWalk<'a>, SqlError> {
    let mut edge_count = 0usize;
    let mark = arena.mark();
    for_each_dependency(storage, txid, arena, &mut |_| {
        edge_count += 1;
        Ok(())
    })?;
    // SAFETY: the counting pass keeps no reference into its temporaries.
    unsafe { arena.rewind_to(mark) };
    let edges = arena
        .alloc_slice_with(edge_count, |_| DependencyEdge {
            dependent: ObjectAddress::new(0, 0, 0),
            referenced: ObjectAddress::new(0, 0, 0),
            kind: 0,
        })
        .map_err(|_| table_full())?;
    let edges_mark = arena.mark();
    let mut filled = 0usize;
    for_each_dependency(storage, txid, arena, &mut |edge| {
        *edges.get_mut(filled).ok_or_else(table_full)? = edge;
        filled += 1;
        Ok(())
    })?;
    // SAFETY: the edges were copied into memory allocated before the mark.
    unsafe { arena.rewind_to(edges_mark) };
    let edges = &edges[..filled];
    // Every edge yields at most one target and one pending dependent.
    let capacity = edges.len() + originals.len();
    let targets = arena
        .alloc_slice_with(capacity, |_| Target {
            object: ObjectAddress::new(0, 0, 0),
            flags: 0,
            dependee: None,
        })
        .map_err(|_| table_full())?;
    let pending = arena
        .alloc_slice_with(2 * edges.len() + 1, |_| (ObjectAddress::new(0, 0, 0), 0u16))
        .map_err(|_| table_full())?;
    let stack = arena
        .alloc_slice_with(capacity.max(1), |_| Frame {
            object: ObjectAddress::new(0, 0, 0),
            flags: 0,
            partition_owner: None,
            first: 0,
            count: 0,
            next: 0,
        })
        .map_err(|_| table_full())?;
    let count = {
        let mut walker = Walker {
            storage,
            txid,
            edges,
            originals,
            targets: &mut *targets,
            target_count: 0,
            pending,
            pending_count: 0,
            stack,
            depth: 0,
            redirects: 0,
        };
        for original in originals {
            walker.find(*original, ORIGINAL, arena)?;
        }
        walker.target_count
    };
    Ok(DependencyWalk {
        targets: &targets[..count],
    })
}

struct Walker<'w> {
    storage: &'w Storage,
    txid: u32,
    edges: &'w [DependencyEdge],
    originals: &'w [ObjectAddress],
    targets: &'w mut [Target],
    target_count: usize,
    pending: &'w mut [(ObjectAddress, u16)],
    pending_count: usize,
    stack: &'w mut [Frame],
    depth: usize,
    /// Nested redirections to internal owners, bounded by the edge count so
    /// a cycle of internal dependencies fails instead of recursing forever.
    redirects: usize,
}

enum Entry {
    /// Already on the stack or among the targets.
    Done,
    /// Redirected to the object that owns it internally.
    Owner(ObjectAddress),
    /// Its dependents are pushed as a new frame.
    Expanded,
}

impl Walker<'_> {
    /// PostgreSQL's `stack_address_present_add_flags`.
    fn on_stack(&mut self, object: ObjectAddress, flags: u16) -> bool {
        let mut present = false;
        for frame in self.stack[..self.depth].iter_mut() {
            if same_object(frame.object, object) {
                if frame.object.sub == object.sub {
                    frame.flags |= flags;
                    present = true;
                } else if frame.object.sub == 0 {
                    present = true;
                }
            }
        }
        present
    }

    /// PostgreSQL's `object_address_present_add_flags`.
    fn in_targets(&mut self, object: ObjectAddress, flags: u16) -> bool {
        let mut present = false;
        for target in self.targets[..self.target_count].iter_mut() {
            if !same_object(target.object, object) {
                continue;
            }
            if target.object.sub == object.sub {
                target.flags |= flags;
                present = true;
            } else if target.object.sub == 0 {
                present = true;
            } else if object.sub == 0 && flags != 0 {
                target.flags |= flags | SUBOBJECT;
            }
        }
        present
    }

    fn add_target(
        &mut self,
        object: ObjectAddress,
        flags: u16,
        partition_owner: Option<ObjectAddress>,
    ) -> Result<(), SqlError> {
        let dependee = if flags & IS_PART != 0 {
            partition_owner
        } else {
            self.depth.checked_sub(1).map(|top| self.stack[top].object)
        };
        let slot = self
            .targets
            .get_mut(self.target_count)
            .ok_or_else(table_full)?;
        *slot = Target {
            object,
            flags,
            dependee,
        };
        self.target_count += 1;
        Ok(())
    }

    /// Starts visiting `object`: stops if it is already reached, redirects
    /// to an internal owner, or pushes a frame of its sorted dependents.
    fn enter(
        &mut self,
        object: ObjectAddress,
        mut flags: u16,
        arena: &Arena,
    ) -> Result<Entry, SqlError> {
        if self.on_stack(object, flags) || self.in_targets(object, flags) {
            return Ok(Entry::Done);
        }
        if pinned(object) {
            let description = describe(self.storage, self.txid, object, arena)?;
            return Err(sql_err!(
                sqlstate::DEPENDENT_OBJECTS_STILL_EXIST,
                "cannot drop {} because it is required by the database system",
                description
            ));
        }
        let mut owner = None;
        let mut partition_owner = None;
        let mut partitioned = false;
        for edge in self.edges {
            if !same_object(edge.dependent, object)
                || (object.sub != 0 && edge.dependent.sub != object.sub)
                // A sub-object's dependency on its own object (a partition
                // key column on its table) never redirects the whole object.
                || (object.sub == 0 && same_object(edge.referenced, object))
            {
                continue;
            }
            match edge.kind {
                b'i' | b'e' => {
                    if self.depth == 0 {
                        if self.originals.contains(&edge.referenced) {
                            return Ok(Entry::Done);
                        }
                        if owner.is_none() || edge.kind == b'e' {
                            owner = Some(edge.referenced);
                        }
                        continue;
                    }
                    if self.on_stack(edge.referenced, 0) {
                        continue;
                    }
                    return Ok(Entry::Owner(edge.referenced));
                }
                b'P' => {
                    partition_owner = Some(edge.referenced);
                    partitioned = true;
                }
                b'S' => {
                    if !partitioned {
                        partition_owner = Some(edge.referenced);
                    }
                    partitioned = true;
                }
                _ => {}
            }
        }
        if let Some(owner) = owner {
            return Err(self.required_by(object, owner, arena));
        }
        if partitioned {
            flags |= IS_PART;
        }
        let first = self.pending_count;
        for edge in self.edges {
            if !same_object(edge.referenced, object)
                || (object.sub != 0 && edge.referenced.sub != object.sub)
                || (same_object(edge.dependent, object) && edge.dependent.sub != 0)
            {
                continue;
            }
            let subflags = match edge.kind {
                b'n' => NORMAL,
                b'a' | b'x' => AUTO,
                b'i' => INTERNAL,
                b'P' | b'S' => PARTITION,
                b'e' => EXTENSION,
                _ => {
                    return Err(sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "unrecognized dependency type {}",
                        edge.kind as char
                    ));
                }
            };
            *self
                .pending
                .get_mut(self.pending_count)
                .ok_or_else(table_full)? = (edge.dependent, subflags);
            self.pending_count += 1;
        }
        self.pending[first..self.pending_count].sort_unstable_by(|left, right| {
            visit_order(self.storage, self.txid, &left.0, &right.0)
        });
        *self.stack.get_mut(self.depth).ok_or_else(table_full)? = Frame {
            object,
            flags,
            partition_owner,
            first,
            count: self.pending_count - first,
            next: 0,
        };
        self.depth += 1;
        Ok(Entry::Expanded)
    }

    /// PostgreSQL's `findDependentObjects`, with an explicit stack.
    fn find(&mut self, object: ObjectAddress, flags: u16, arena: &Arena) -> Result<(), SqlError> {
        let base = self.depth;
        // The next object to visit: the original, then each pending dependent.
        let mut request = Some((object, flags));
        loop {
            if let Some((object, flags)) = request.take() {
                match self.enter(object, flags, arena)? {
                    Entry::Done => {}
                    Entry::Owner(owner) => {
                        self.find_owner(owner, arena)?;
                        if !self.in_targets(object, flags) {
                            return Err(sql_err!(
                                sqlstate::INTERNAL_ERROR,
                                "deletion of an owning object did not reach its member"
                            ));
                        }
                    }
                    Entry::Expanded => {}
                }
            }
            if self.depth == base {
                return Ok(());
            }
            let top = self.depth - 1;
            let frame = self.stack[top];
            if frame.next < frame.count {
                self.stack[top].next += 1;
                request = Some(self.pending[frame.first + frame.next]);
                continue;
            }
            self.depth -= 1;
            self.pending_count = frame.first;
            // Flags added while the frame was on the stack belong to it.
            let flags = self.stack[top].flags;
            self.add_target(frame.object, flags, frame.partition_owner)?;
        }
    }

    /// Visits an internal owner as if the reference had named it, flagged as
    /// a reverse dependency (never a reason for automatic deletion).
    fn find_owner(&mut self, owner: ObjectAddress, arena: &Arena) -> Result<(), SqlError> {
        if self.redirects > self.edges.len() {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "catalog dependencies form an internal ownership cycle"
            ));
        }
        self.redirects += 1;
        let found = self.find(owner, REVERSE, arena);
        self.redirects -= 1;
        found
    }

    fn required_by(&self, object: ObjectAddress, owner: ObjectAddress, arena: &Arena) -> SqlError {
        required_by(self.storage, self.txid, object, owner, arena)
    }
}

/// PostgreSQL's refusal to drop `object` on its own while `owner` owns it
/// internally (a view's `_RETURN` rule).
pub(crate) fn required_by(
    storage: &Storage,
    txid: u32,
    object: ObjectAddress,
    owner: ObjectAddress,
    arena: &Arena,
) -> SqlError {
    let mark = arena.mark();
    let error = (|| {
        let object = describe(storage, txid, object, arena)?;
        let owner = describe(storage, txid, owner, arena)?;
        crate::sql::eval::stash_diagnostic(
            None,
            Some(stack_format!(128, "You can drop {} instead.", owner)),
        );
        Ok::<_, SqlError>(sql_err!(
            sqlstate::DEPENDENT_OBJECTS_STILL_EXIST,
            "cannot drop {} because {} requires it",
            object,
            owner
        ))
    })()
    .unwrap_or_else(|error| error);
    // SAFETY: the error copied both descriptions.
    unsafe { arena.rewind_to(mark) };
    error
}

/// PostgreSQL's `getObjectDescription` of one reached object.
pub(crate) fn describe<'a>(
    storage: &Storage,
    txid: u32,
    object: ObjectAddress,
    arena: &'a Arena,
) -> Result<&'a str, SqlError> {
    let catalog = crate::sql::query::storage_catalog(storage, arena, txid);
    crate::sql::eval::funcs::system::describe_object_text(
        &catalog,
        object.class,
        object.object,
        object.sub,
        arena,
    )?
    .ok_or_else(|| {
        sql_err!(
            sqlstate::INTERNAL_ERROR,
            "dependency report cannot describe catalog object {}/{}/{}",
            object.class,
            object.object,
            object.sub
        )
    })
}

/// PostgreSQL's `reportDependentObjects`: fails a `RESTRICT` drop that
/// reaches other objects with one `DETAIL` list, or prepares a `CASCADE`
/// drop's one notice. The report also holds the filter the statement's
/// executors use to confirm they drop nothing it left out.
pub(crate) fn report(
    storage: &Storage,
    txid: u32,
    walk: &DependencyWalk<'_>,
    originals: &[ObjectAddress],
    cascade: bool,
    arena: &Arena,
) -> Result<crate::pg::respond::DependencyReport, SqlError> {
    let mut detail = StackStr::<MAX_DIAGNOSTIC_DETAIL_BYTES>::new();
    let mut filter = crate::pg::respond::DependencyReportFilter::EMPTY;
    let mut reported = 0usize;
    let mut unreported = 0usize;
    let mut refused = false;
    // An object with partition dependencies goes only with one of them.
    for target in walk.targets {
        if target.flags & IS_PART != 0 && target.flags & PARTITION == 0 {
            let owner = target.dependee.ok_or_else(|| {
                sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "partition dependency has no owner"
                )
            })?;
            let object = describe(storage, txid, target.object, arena)?;
            let owner = describe(storage, txid, owner, arena)?;
            crate::sql::eval::stash_diagnostic(
                None,
                Some(stack_format!(128, "You can drop {} instead.", owner)),
            );
            return Err(sql_err!(
                sqlstate::DEPENDENT_OBJECTS_STILL_EXIST,
                "cannot drop {} because {} requires it",
                object,
                owner
            ));
        }
    }
    for target in walk.targets.iter().rev() {
        if target.flags & ORIGINAL != 0 {
            continue;
        }
        let mark = arena.mark();
        let silent = target.flags & (SUBOBJECT | SILENT) != 0;
        // A silent object pos3ql cannot describe (an operator class member)
        // stays out of the filter; an executor that reports it fails loudly.
        let description = match describe(storage, txid, target.object, arena) {
            Ok(description) => description,
            Err(_) if silent => {
                // SAFETY: nothing allocated since the mark is used.
                unsafe { arena.rewind_to(mark) };
                continue;
            }
            Err(error) => return Err(error),
        };
        filter.insert(description);
        if silent {
            // SAFETY: the description is not used past this point.
            unsafe { arena.rewind_to(mark) };
            continue;
        }
        if reported == MAX_REPORTED {
            unreported += 1;
            // SAFETY: the description is not used past this point.
            unsafe { arena.rewind_to(mark) };
            continue;
        }
        if reported != 0 {
            let _ = detail.write_char('\n');
        }
        if cascade {
            let _ = write!(detail, "drop cascades to {}", description);
        } else {
            refused = true;
            let dependee = match target.dependee {
                Some(dependee) => describe(storage, txid, dependee, arena)?,
                None => "",
            };
            let _ = write!(detail, "{} depends on {}", description, dependee);
        }
        reported += 1;
        // SAFETY: the descriptions were copied into `detail`.
        unsafe { arena.rewind_to(mark) };
    }
    if unreported != 0 {
        let _ = write!(
            detail,
            "\nand {} other objects (see server log for list)",
            unreported
        );
    }
    if detail.is_truncated() {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "dependency report exceeds its fixed buffer"
        ));
    }
    if refused {
        crate::sql::eval::stash_diagnostic(
            Some(detail),
            Some(StackStr::from_str(
                "Use DROP ... CASCADE to drop the dependent objects too.",
            )),
        );
        return Err(if let [original] = originals {
            let mark = arena.mark();
            let description = describe(storage, txid, *original, arena)?;
            let error = sql_err!(
                sqlstate::DEPENDENT_OBJECTS_STILL_EXIST,
                "cannot drop {} because other objects depend on it",
                description
            );
            // SAFETY: the error copied the description.
            unsafe { arena.rewind_to(mark) };
            error
        } else {
            sql_err!(
                sqlstate::DEPENDENT_OBJECTS_STILL_EXIST,
                "cannot drop desired object(s) because other objects depend on them"
            )
        });
    }
    Ok(crate::pg::respond::DependencyReport {
        filter,
        notice: (reported != 0).then_some((detail, reported + unreported)),
    })
}

/// Walks and reports a `DROP` of `originals`. The returned report holds the
/// `CASCADE` notice and lets the statement's executors confirm their own
/// cascade lines against it; it is `None` inside a drop another statement's
/// report covers.
pub(crate) fn report_drop(
    storage: &Storage,
    txid: u32,
    originals: &[ObjectAddress],
    cascade: bool,
    arena: &Arena,
    responder: &mut crate::pg::respond::Responder<'_>,
) -> Result<Option<crate::pg::respond::DependencyReport>, SqlError> {
    if responder.dependency_report().is_some() {
        return Ok(None);
    }
    let mark = arena.mark();
    let reported = walk(storage, txid, originals, arena).and_then(|walked| {
        let reported = report(storage, txid, &walked, originals, cascade, arena)?;
        crate::sql::event_trigger::record_drop_order(
            walked
                .targets
                .iter()
                .rev()
                .map(|target| (target.object.class, target.object.object, target.object.sub)),
        );
        Ok(reported)
    });
    // SAFETY: the report is a value; nothing borrows the walk's memory.
    unsafe { arena.rewind_to(mark) };
    reported.map(Some)
}

/// Sends a cascade report an executor built for its own part of a drop:
/// `detail` holds `count` lines of `drop cascades to ...`. Inside a statement
/// whose dependency report was already sent, each line must name an object
/// that report reached; nothing is sent.
pub(crate) fn send_cascade_lines(
    responder: &mut crate::pg::respond::Responder,
    detail: &str,
    count: usize,
) -> Result<(), SqlError> {
    if count == 0 {
        return Ok(());
    }
    if responder.dependency_report().is_some() {
        responder
            .send_dependency_notice()
            .map_err(|_| sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "response buffer full"))?;
        let filter = responder
            .dependency_report()
            .expect("the running drop keeps its report");
        for line in detail.split('\n') {
            let object = line.strip_prefix("drop cascades to ").unwrap_or(line);
            if !filter.contains(object) {
                return Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "dependency report omitted {}",
                    object
                ));
            }
        }
        return Ok(());
    }
    let full = |_| sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "response buffer full");
    if count == 1 {
        responder
            .notice(sqlstate::SUCCESSFUL_COMPLETION, detail)
            .map_err(full)
    } else {
        crate::sql::eval::stash_diagnostic(Some(StackStr::from_str(detail)), None);
        responder
            .notice(
                sqlstate::SUCCESSFUL_COMPLETION,
                stack_format!(128, "drop cascades to {} other objects", count).as_str(),
            )
            .map_err(full)
    }
}
