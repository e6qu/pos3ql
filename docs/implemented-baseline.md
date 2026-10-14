# What works today

This is a summary of implemented behavior. The [plan](../PLAN.md) describes
what still needs to be built or demonstrated. Statement execution remains
serial, and representative production performance has not been established.

## From an application's point of view

| You want to… | What is implemented | Where to check the limits |
|---|---|---|
| Connect using PostgreSQL tools | SQL, transactions, catalogs, drivers, and dump/restore, compared with PostgreSQL 18 | [Compatibility](postgresql-18-compatibility.md) |
| Save data outside the database machine | Object-store commits and checkpoints; success waits for durable publication | [Object storage](object-storage.md) |
| Find selected rows without reading every object | Btree, hash, BRIN, GiST, GIN, and SP-GiST access paths with exact result checks | [Indexes](index-navigation.md) |
| Recover after losing local caches | Recovery from saved objects and the durable commit history | [Formats](durable-format.md) |
| Save a recovery point or make a separate copy | Named backups, export to another prefix, recovery to an LSN or timestamp | [Backup and restore](backup-restore.md) |
| Run and replace the server | Probes, metrics, capacity reporting, credential reload, packages, and passive-candidate promotion | [Operations](operations.md) |
| Maintain another database's copy of selected tables | PostgreSQL logical publication/subscription and pgoutput interoperability | [Logical replication](logical-replication.md) |
| Query JSON or XML values | Typed JSON/JSONB/jsonpath and SQL/JSON; XML with an explicit XPath subset | [JSON](sql-json.md), [XML](sql-xml.md) |

For example, a successful durable commit can be recovered with empty RAM and
local disk caches. A logical subscriber can receive an order update, but it
may lag behind the publisher. An unsupported XPath expression returns an
error rather than quietly giving a different answer.

## Runtime ownership

This section explains the recent concurrency preparation. “Ownership” means
keeping data and buffers valid until the operation using them has finished.
It does not mean that SQL statements already execute in parallel.

**User example:** transaction A reads an order, transaction B updates it, and
A reads again. Under repeatable-read isolation, A still needs its earlier
version. Background cleanup must keep those bytes and objects until A releases
that version. Read-committed statements may select a newer version on their
next statement.

| Shared state | Protection implemented | Work still needed for concurrent execution |
|---|---|---|
| Statement buffers | Startup-sized arenas, row-selection scratch, connection/database context, and ordered dispatcher leases; COPY and subscriptions have separate buffers | Run statements on workers and coordinate all remaining shared state |
| Transaction identities | One guarded counter serves SQL, maintenance, and recovery. Creating a SQL transaction reserves its ID and registry entry together; capacity errors leave existing transactions valid. | Coordinate journal staging, prepared transactions, and engine publication during concurrent execution. |
| Row identities | Shared allocation reserves unique IDs without runtime allocation. Inserts and partition moves attach deferred checks to the ID actually stored; exhaustion returns `54000` before appending bytes. | Transaction publication and execution workers remain unfinished. |
| Catalogs and table definitions | Synchronized updates; readers retain the definition and table identity they selected; nested lookups release shared guards | Concurrent creation/removal and retirement of table state |
| Sequence positions | Per-table synchronization, checked arithmetic, coherent journal/checkpoint images, and acknowledgement of unchanged staged positions | Preserve these rules through engine-wide concurrent publication |
| Row versions | Pending writes, rollback, committed publication, row maps, and visibility caches use guarded state. Point metadata reads own bounded copies of their chains and byte sources, allowing unrelated publication and later selection after removal or shutdown | Safe movement and reclamation while readers remain active |
| Planner statistics | Readers copy their visible estimates; private versions and saved-state markers use guarded cells and fixed pools | Integrate these captures with concurrent engine publication |
| Row bytes | Selected rows own their startup heap allocation; reclamation can copy live rows into an unretained buffer | Integrate relocation with concurrent table publication and maintenance scheduling |
| Read positions | Statement and transaction retention; replication export validity and import protection share one guard, with separate fixed capacities | Integrate every remaining table lifecycle path and engine publication |
| Deferred reads | Logical row identity, table identity, and exact version identity, rather than remembered heap addresses | Coherent table lifecycle changes across every read path |

### How long reads keep data alive

Executing SQL and large-object fast-path statements register an owned snapshot
lease: a reservation recording the read position they must preserve. Each
query workspace reserves 1,024 slots at startup. Registration happens under
the same synchronization as row publication, so it cannot miss an intervening
commit. Nested statements do not replace a transaction's snapshot. Completion,
errors, and transaction end release retention, wake schema waiters, and
schedule pruning.

Streaming `COPY FROM` keeps its lease in the connection's setup until completion
or abort and restores the captured read position for each chunk. `COPY TO`,
buffered portals, and SQL cursors finish their storage reads before suspension.
Table `COPY TO` captures visible resident versions, its schema, and saved
files before ordering its output records. It fetches original bytes through those
owners, without consulting live version chains or table schemas during output.
Its source owner releases heap/files on completion or error. Saved row payloads are fetched
when emitted; the export does not copy every payload before starting output.
An output record uses 32 bytes on the release target, plus the scan's resident
records and file list. Exhaustion returns `54000`.
Capture, row iteration, and output check the statement deadline; expiration
returns `57014` and releases the export's sources.

**User example:** `COPY orders (id, note) TO STDOUT (FORMAT csv)` exports the
versions collected for that statement, including its visible pending changes.
A savepoint rollback restores earlier rows for the next export. Use
`COPY (SELECT id, note FROM orders ORDER BY id) TO STDOUT` when a particular
row order is required; physical scan order is not a PostgreSQL SQL guarantee.
Table-form exports read only the named table. To include partitions or inherited
children, use the query form. A partitioned parent without applicable row security
returns `42809`; PostgreSQL's row-security rewrite instead exports its own empty
row set. Plain views and foreign tables require the query form. Populated
materialized views can be copied directly; unpopulated ones return `0A000`.
These relation boundaries follow [PostgreSQL 18 COPY](https://www.postgresql.org/docs/18/sql-copy.html)
and its [row-security rewrite](https://github.com/postgres/postgres/blob/REL_18_STABLE/src/backend/commands/copy.c).
Direct catalog-table COPY is an explicit implementation limit; use the query form.

Checkpoint and vacuum keep data through their own maintenance sources.

Query walks over immutable sorted tables (SSTs) retain the selected objects,
schema, read position, and reader buffers. The 64 source slots per workspace
are reserved at startup. An already captured source remains valid after its
table or Storage owner is retired. Capturing or releasing a source holds shared
registry state briefly; object I/O and scan callbacks do not hold that registry.
Scans capture their saved-file list, schema, and read position before running
callbacks. The fast path for tables entirely in saved files owns its source
without borrowing Storage; it rejects a resident overlay before reserving scan
memory. Unread saved rows remain available after table replacement or Storage
shutdown. The sequential SQL path described below also owns resident selections.
Rows kept in the in-memory map also
resolve saved versions from those files; changing a file's position in the live
list cannot redirect a selected read. Version ownership ends before object I/O.
Checkpoint cursors follow the same
capture order. Selected resident row bytes own the startup heap allocation;
retaining them does not keep a heap read guard held. They remain readable after
table removal, reuse of its slot, or Storage teardown. Retaining a read does not
copy a row or allocate memory. Two heap buffers are reserved at startup, each
`memtable_bytes` in size. Reclamation moves bytes in place when the current
buffer is unretained; otherwise it copies live rows into the free buffer and
preserves the old one for readers. Current, historical, and pending row
locations change together, and old detached locations remain invalid.
If readers retain both buffers, reclamation returns `54000` before changing
bytes or locations and can be retried after readers release them. Invalid row
requests, cancellation, and statement-buffer exhaustion release ownership.

**User example:** once a read has selected an order's in-memory contents,
removing the table cannot replace those contents with a newly created table's
row. Reclamation can free space for new rows while that read keeps its old
bytes. With `memtable_bytes = 64MiB`, heap bytes reserve 128 MiB before other
startup controls. Sequential SQL byte scans select resident versions during
capture, before fetching those bytes. Authoritative metadata walks, remaining
executor snapshot reads, and concurrent table publication still need coordination.

Individual object-row lookups retain their full generation list, schema,
table identity, and reader buffers through byte retrieval. Their metadata also
represents a newest deletion, so an older transaction can still resolve the
preceding saved version after resident history disappears. Latest-committed
constraint checks probe the captured objects beyond an older scan's read
position instead of treating that scan's selected row as the latest version.
Captured sources remain readable after table replacement and Storage teardown.
The last shared lease releases the roots and makes its slot reusable.

Sorted scans and enum rewrites retain one source for their selected heap
allocation and saved files. Their copyable row records borrow that source,
keeping physical order, version, and byte location together. Fetching a record
uses its original owner directly, so removal, version-chain reuse, and heap relocation cannot redirect
the read. Changed heap allocations or saved-file sources fail with `40001`
before retention. Each optional row record keeps the existing 32-byte size in
the fixed statement arena on the release target; source exhaustion retains the
existing `54000` contract. Object rows no longer repeat a metadata lookup before
fetching bytes.

**User example:** a report scanning a table entirely from saved files keeps its
earlier orders even if later versions delete them. Replacing the table cannot
redirect the report's next unread row to the new table's schema or files.
For a sorted read, collected rows keep their original bytes even if reclamation
moves the current table's rows before those collected rows are fetched.
For example, `ALTER TYPE order_state RENAME VALUE 'pending' TO 'waiting'`
retains the rows it collected while rewriting scalar and array values. A
savepoint rollback restores their earlier labels and indexed lookups; an
exhausted statement arena returns `54000` and rolls back the alteration.
MERGE copies each target row's selected bytes into the existing statement arena
before policy evaluation or writes. Later decoding uses those copies, including
for inherited columns and partition moves. It no longer looks up a saved
version stamp against a mutable row map.

Index creation uses the same captured byte owners, but validates current
committed rows and all earlier writes by the creating transaction. It does not
change which versions ordinary transaction queries see.

**User example:** inside `BEGIN`, inserting two orders with the same reference
and then creating a unique index must fail with `23505`. Updating or deleting
an order before creating the index checks the replacement values. A failed
creation releases its row sources and pending catalog entry; rolling back to a
savepoint allows a corrected creation with the same name. Later uniqueness
checks also use own replacements and deletions: deleting an order lets that
transaction reuse its reference before commit, including through a saved index.
An older repeatable
read transaction cannot create a unique index that misses later committed
duplicates. The [PostgreSQL 18 index-build implementation](https://github.com/postgres/postgres/blob/REL_18_STABLE/src/backend/access/heap/heapam_handler.c)
is the upstream visibility reference; the differential suite checks actual
PostgreSQL results.

Internal metadata/byte scans also reserve 64 retention slots per workspace.
Captured row identity lists own their startup pool slot, so their numbers remain
available after the source map, table, or Storage owner is removed. Concurrent
captures reserve distinct slots; failed capture and final release clear a slot
before reuse. Iteration and nested reads use only that slot's private guards.
This preserves the list's memory; authoritative metadata walks still consult
live resident versions.

Sequential SQL byte scans capture each resident row's visible version, byte
location, and deletion status during one metadata transition. A shared source
owns the original heap allocation, saved-file handles, schema, and read position.
Fetching an unread row does not revisit mutable version chains, even after
rollback, table replacement, relocation, or Storage teardown. Saved rows outside
that resident overlay continue streaming through the existing merged file reader,
including selected-column decoding. Saved-history lookups, downloads, and callbacks
run after the table and version guards are released.

**User example:** a report can pause after its first in-memory order while a
writer changes another order. The report's next row still uses the version it
captured, even if cleanup reuses that version's old metadata slot.

Resident scan records use 32 bytes each in the fixed statement arena, replacing
the former 8-byte identity list; saved rows are not copied into that list.
Exhaustion returns `54000` and releases captured heap/file ownership. No startup
pool or statement limit grows to rescue the query. Nested wide joins continue
using arena-owned metadata rather than one scan-source slot per join edge.
Registered files release before statement memory is reset; an object-row source
selected from the scan copies ownership into a startup slot. Checked reader
identities and full pools return `54000` before reuse or partial registration.
These are separate reservations;
the [capacity inventory](postgresql-18-compatibility.md#capacity-boundaries)
records their limits.

Point sources reserve another 64 slots per workspace, independent of the 64
scan-source slots. Sharing one captured source uses checked reader counts;
exhaustion returns `54000` without partial retention. Row-fetch scratch and
consume-in-place byte limits also fail with `54000`, including nested fetches;
errors release the request buffers and retained sources. These reservations are
included in the startup memory plan.

The installed generation list groups each file handle with the commit boundary
used for memory eviction. A short guard keeps replacement from interleaving
with capture and root registration; downloads and row callbacks run after that
guard ends. New lists use a second fixed startup buffer, so an invalid or
oversized installation preserves the prior list. Appending to a full list
returns `54000` without changing installed members. Database clones retain both
handles and boundaries.

**User example:** a read keeps the complete set of saved order files it selected
while maintenance replaces that set. A new capture sees a complete replacement,
never half of the old list mixed with half of the new one. Coordinating this
with rows still in memory and table removal remains planned work.

**User example:** a report reads some orders from memory and others from saved
files. Maintenance replaces those files while the report processes its first
rows. The report continues with its original file set and read position; a new
scan captures the replacement. Downloads and callbacks hold none of the table
capture guards. Cancellation and failed capture release reader registrations.

### How statistics reads and journal batches stay consistent

`ANALYZE` collects estimates used to choose query plans. Relation counts and
column estimates have different PostgreSQL lifetimes: rolling back can leave
`pg_class.reltuples` changed while `pg_stats` column estimates revert. Reads
copy the image visible to their transaction under short guards. The private
version pool, rollback, commit, replay, and table reuse use the same ownership
order. Readers retain their copied values without holding these guards.

Journal capture copies the visible estimates and records the exact relation-count
revision from the same cell. Acknowledgement clears that captured revision's
pending marker; a newer revision remains pending. Reused table slots preserve
revision identities, and an owned receipt cannot redirect to another Storage
instance. Each query workspace reserves receipts for every physical table at
startup. Full or already retained workspaces return `54000`; cancellation and
staging errors release them. Guards end before journal encoding and object I/O.

**User example:** an older journal batch has captured an order table's row count.
A later `ANALYZE` changes that count before the older batch finishes. Finishing
the old batch leaves the new count marked for a later journal batch. Changes in
one database are captured only when that database journals them.

### What maintenance may change

Resumable value-index checkpoint jobs keep a startup-sized `table_rows` buffer
of row identities and completed work. Moving a row between buckets or rolling
back a write does not change its logical resume position. Eviction sends
unprocessed rows through paced SST merging. Jobs restart if committed input or
the table identity changes.

Full SST rewrites and adjacent-generation merges remap retained committed
versions, including their exact commit positions and deletion markers. Paced
merges keep the historical baseline, and byte scans skip versions newer than
the selected snapshot. Complete rewrites with historical object versions remain
explicitly guarded until a complete historical source exists. Merge scratch
exhaustion returns `54000`.

A startup-sized registry protects durable and temporary object graphs used by
readers. Garbage collection copies retained root references before object I/O.
Releasing a root used by a durable garbage pass schedules more maintenance even
without another write. Active backups also retain their reachable objects.

### What publication may change

Row publication prepares keys before taking metadata ownership and verifies
that the selected pending version is still current. Its acquisition order is
snapshot, table, version, row, then cache. Cache probes retain bounded candidate
identities and release cache ownership before callbacks; a newer cached image
cannot certify an older snapshot.

Metadata-only ALTER keeps existing rows and immutable objects when their
representation is unchanged. For example, adding a UNIQUE constraint still
checks existing and transaction-modified rows; it does not needlessly rewrite
those rows. Operations that change row representation retain the rewrite path.

Appends release byte ownership before publishing metadata. Compaction checks
its complete relocation set before changing bytes or handles. Readers reject
reused table identities and stale heap locations. Heap addresses and relocation
generations describe caches, not durable row identity. Retained byte copies
use statement memory. Physical reclamation still requires exclusive table
metadata, but retained byte readers can continue using the old heap buffer.

Transaction-wide durable publication, concurrent cache/object I/O and
maintenance, table lifecycle changes, and execution workers are still open.
A retained reader must remain safe across all of those changes.

## What the tests establish

Tests forbid runtime allocation, compare SQL and wire behavior with PostgreSQL,
exercise real drivers, recover without local caches, and inject reproducible
storage failures. See [contributing](../CONTRIBUTING.md) for required checks.

Performance tests use the instrumented fixture, MinIO, SeaweedFS, and vanilla
PostgreSQL 18 with available and matched CPU/memory. Existing measurements
establish tested behavior and exploratory costs. They do not establish
production latency, throughput, or availability across machines.

Past implementation narratives and their evidence remain in
[history](history/README.md).
