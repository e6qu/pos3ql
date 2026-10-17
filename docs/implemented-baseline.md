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
| Journal positions | Checked shared reservations and observations that never move backward; staging exhaustion follows ordinary rollback paths; exact replay restoration stays exclusive | Coordinate durable commit ordering and journal buffers during concurrent execution |
| Row identities | Shared allocation reserves unique IDs without runtime allocation. Inserts and partition moves attach deferred checks to the ID actually stored; exhaustion returns `54000` before appending bytes. | Transaction publication and execution workers remain unfinished. |
| Catalogs and table definitions | Synchronized updates; readers retain their selected definition and identity; each table has its own cleanup lock; transactional creation reserves names and slots through shared storage access | Engine-wide publication, rewrites, and maintenance integration |
| Sequence positions | Per-table synchronization, checked arithmetic, coherent journal/checkpoint images, and acknowledgement of unchanged staged positions | Preserve these rules through engine-wide concurrent publication |
| Row versions | Pending writes, rollback, committed publication, row maps, and visibility caches use guarded state. Point metadata reads own bounded copies of their chains and byte sources, allowing unrelated publication and later selection after removal or shutdown | Safe movement and reclamation while readers remain active |
| Planner statistics | Readers copy their visible estimates; private versions and saved-state markers use guarded cells and fixed pools | Integrate these captures with concurrent engine publication |
| Row bytes | Selected rows own their startup heap allocation; reclamation can copy live rows into an unretained buffer | Integrate relocation with concurrent table publication and maintenance scheduling |
| Read positions | Statement and transaction retention; replication export validity and import protection share one guard, with separate fixed capacities | Integrate every remaining table lifecycle path and engine publication |
| Deferred reads | Scans and rewrites keep their original byte sources; UPDATE, DELETE, and MERGE copy selected values into fixed statement memory | Finish coherent table lifecycle changes |

### How long reads keep data alive

Executing SQL and large-object fast-path statements register an owned snapshot
lease: a reservation recording the read position they must preserve. Each
query workspace reserves 1,024 slots at startup. Registration happens under
the same synchronization as row publication, so it cannot miss an intervening
commit. Nested statements do not replace a transaction's snapshot. Completion,
errors, and transaction end release retention, wake schema waiters, and
schedule pruning.

Streaming `COPY FROM` keeps its lease in the connection's setup until completion
or abort and restores the captured read position for each chunk. When statement
triggers need the inserted values, COPY encodes them after BEFORE triggers,
generated-column calculation and storage, before AFTER row triggers. Each
connection and subscription worker reserves `copy_transition_bytes` at startup
(default 256 KiB), plus `txn_rows` records of two machine-sized offsets. The
values survive message boundaries and arena resets without later heap, object,
or remote-row lookup. COPY without these triggers does not consume that buffer.
Trigger queries also need their existing statement memory: `work_arena_bytes`
for client COPY, or `subscription_arena_bytes` for a subscription worker.
Overflow returns `54000` and aborts the COPY. Completion, errors, cancellation,
connection reuse and subscription cleanup release the bytes for the next COPY.

**User example:** an AFTER trigger updates an order that COPY just inserted.
A statement trigger inspecting the inserted rows still receives the values
that COPY stored. If its fixed buffer fills, the COPY fails and its transaction
can roll back and retry a smaller batch.

`COPY TO`,
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
capture, before fetching those bytes. Remaining executor paths that save only a
version number, concurrent table publication, and historical saved-file rewrites
still need coordination.

Individual object-row lookups retain their full generation list, schema,
table identity, and reader buffers through byte retrieval. Their metadata also
represents a newest deletion, so an older transaction can still resolve the
preceding saved version after resident history disappears. Latest-committed
constraint checks probe the captured objects beyond an older scan's read
position instead of treating that scan's selected row as the latest version.
Internal metadata walks validate the table's captured identity while holding
the same brief definition guard used to retain each row's metadata and original
byte owners. A reused table slot returns `40001` before a new reader slot is
reserved. Definition and shared row/version guards release before callbacks,
so an unrelated writer can progress while a callback retains bytes. Pending
changes select bytes through the retained metadata owner, including after
heap relocation. Saved-row rereads and commit key encoding also use the
captured byte owner. Commit publication validates the pending version and uses
its current location if reclamation moved it. Current row versions within the
same table remain visible to authoritative walks. History deletion holds the
registry and shared version guards for at most 64 captured row identities at a
time. One brief row-map read copies each table's identities before its batches
begin. It rechecks current readers before each batch. Capturing the next
table's identities holds neither the registry nor the shared version pool while
waiting for its row map. New statement admission coalesces cleanup already in
progress and leaves later release notifications pending for the next attempt.
A new report admitted between cleanup batches still keeps the versions it needs.

**User example:** finishing a report can reclaim its old order versions without
keeping an unrelated customer update waiting for every table to be visited.

Pending table definitions retain their table and one reserved version slot.
A reader releases the pool's allocation control before returning the definition;
another table can publish, roll back, or retire its own versions while that reader
keeps its selection. Slot controls and version capacity are charged at startup.
A full pool or version history returns `54000` before changing the published
head. Rolling back or removing a table releases only its own chain of versions.
Pending edits and savepoint rollback own only the selected table. They compose
column mappings from the original committed layout and update comment names with
the selected shape. Waiting for that table's readers does not reserve the shared
version allocator or comment catalog. Rule history and pending shapes check all
transaction owners under the same definition guard. Capacity or comment-ownership
failure leaves the prior shape and comment names intact. Comment text publication
preserves name changes owned by another transaction.

**User example:** after moving a table to another schema and rolling back to a
savepoint, its constraint comments remain attached under the original name. A
constraint renamed before the savepoint keeps its new name. Both journal replay
and checkpoint recovery preserve the committed result after local caches are lost.

Table-name lookup first checks a small name view for each table. A definition
writer on an unrelated table does not hold that view while doing its work.
Only a matching candidate needs the full definition guard; lookup rechecks its
database, creation stamp and visible name before returning it. Definition writers
publish the updated view before releasing their definition owner. Committed,
transaction-private and rolled-back names follow the same visibility rules as
the definitions. Replay, database cloning and exclusive schema changes update
the view through scoped writes. The views and pending name fields are inline in
the table slots and included in startup memory; lookups allocate nothing.

**User example:** checking an order's customer reference can find the customers
table while an unrelated table definition is being changed. Looking for a table
that does not exist also avoids waiting for unrelated definitions. SQL execution
is still serial; this removes a storage-level dependency before workers are added.
Committed definition changes, table renames, rewrites and maintenance coordination
remain in [PLAN.md](../PLAN.md).

Client descriptions and data-changing WITH queries use the same DML output
analysis, including view-column rewriting and original output names. Their target definition comes from
the existing startup-owned image pool. Live table and pending-schema guards
release before nested result-type lookup; a WITH query no longer puts a complete
wide target definition into its statement arena just to describe its output. Reading
the WITH result reserves its own relation schema in the statement buffer.
Exhaustion returns `54000` and rolls back the change; a narrower query can retry
within the same startup budget.

For a view that calls an order's `amount` column `total`, an update inside a
WITH query returning `total` reports that name and the original numeric type.
Preparing or describing that query determines its columns without updating an order.
`DEALLOCATE ALL` releases client-prepared names as well as SQL PREPARE names.
Already bound portals keep their original statements until closed; replacement
names cannot redirect their execution. Repeated client cache resets reuse the
same fixed pool.

**User example:** a storage check started for an old orders table cannot start
reading the replacement orders table through the reused internal slot. Bytes
it already selected stay readable; a later admission fails explicitly.

ALTER TABLE rewrites retain the source before choosing any input row identity.
The chosen versions and their later reads therefore share one source through
row changes, rollback, and maintenance. An ALTER that changes only a definition
checks for a first row when needed; it does not reserve one rewrite record for
every saved row.

**User example:** renaming a column in a large saved orders table does not fail
because the server's update workspace cannot hold every order. A type change
that really rewrites those orders still obeys its fixed row and statement limits.

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

Ordinary local UPDATE and DELETE also retain selected bytes in the existing
statement buffer before releasing their original sources. Sorting target
identities or reusing nested trigger scratch cannot redirect a later fetch.
Foreign tables keep their remote tuple-identity contract. A plain DELETE avoids
copying old values when no projection, trigger, or referencing key needs them.
Query predicates and `RETURNING` use the named table's column order; stored
bytes, row triggers, generated columns, and constraints use each child's
physical column order, including attached partitions with reordered columns.
Foreign-key actions map referenced columns by name and retain matching child
bytes in one scan. MERGE retains each selected row's schema and values; a
trigger cannot silently replace that row before the outer change writes it.

SQL UPDATE and MERGE moves run BEFORE UPDATE on the original partition, then
DELETE and INSERT row triggers on the old and new partitions. Generated values
are computed after BEFORE triggers from the destination definition. Constraints,
`RETURNING`, and statement-trigger input rows see the resulting values.
For example, a destination partition may calculate a stored total differently
from its parent; inserting through the parent still uses that destination's
calculation. See [column-order examples](postgresql-18-compatibility.md#partition-column-order)
and [replication behavior](logical-replication.md#subscriber).

**User example:** `UPDATE parent SET amount = amount + 1` preserves a child's
extra columns after `ALTER TABLE parent ADD COLUMN`, or after attaching an
existing child whose columns are in a different order. Child constraints still
apply, while a primary key declared only on the parent does not become a child
constraint.

**User example:** a BEFORE trigger changes another order already selected by
the same UPDATE. PostgreSQL returns `27000` and rolls back that statement;
silently overwriting the trigger's change would be wrong. AFTER triggers remain
queued until the main changes finish. When an UPDATE joins the orders table to
itself, a later order reads its source values from the statement's earlier view.
Likewise, `UPDATE orders SET amount = amount + 1 RETURNING amount,
(SELECT sum(amount) FROM orders)` returns the changed row amount while its
ordinary subquery reads the statement's earlier data.

Nested trigger SQL receives a checked command identity and read scope. The
caller regains its command and read view after success or failure, while issued
identities remain consumed. Buffer or identity exhaustion returns `54000` and
allows rollback. The PostgreSQL executor source linked below documents the
distinction between a repeated write and a write made by triggered SQL.

`INSERT ... ON CONFLICT` copies the matched current row into the existing
statement buffer before releasing its original source. It includes earlier own
changes and committed rows recovered entirely from object storage. Index
expression scratch is reused between examined rows; an oversized matched row
fails with `54000` before changing data.

**User example:** a repeatable-read report still sees an order's old reference
after another session changes it. An insert that conflicts with the newer row
must fail with `40001`, including `DO NOTHING`; it cannot silently update or
ignore a row outside that transaction's view. Two proposed rows cannot update
the same existing row in one statement: PostgreSQL returns `21000` and rolls
back that statement. `DO UPDATE ... WHERE false` still locks its conflicting
row, while unrelated orders can change. Savepoint rollback releases that lock.
The [PostgreSQL 18 conflict executor](https://github.com/postgres/postgres/blob/REL_18_STABLE/src/backend/executor/nodeModifyTable.c)
defines the ordering of visibility, locking, filtering, and policy checks;
two-session comparisons verify actual results.

Index creation uses the same captured byte owners, but validates current
committed rows and all earlier writes by the creating transaction. It does not
change which versions ordinary transaction queries see. Ordinary index creation,
recursive partition indexes, and indexes copied by `LIKE INCLUDING INDEXES`
share validation, cache preparation, journal staging, and failure cleanup.
Validation recycles expression scratch after each row and captured sources after
each table. Failed cache preparation releases partial bindings and preserves
earlier visible indexes when restoration succeeds; a failed restoration returns
an error without advertising partial lookup results. Saved-index probes
retain recent row bytes and release shared row-state guards before reading or
returning each key.

**User example:** inside `BEGIN`, inserting two orders with the same reference
and then creating a unique index must fail with `23505`. Updating or deleting
an order before creating the index checks the replacement values. A failed
creation releases its row sources and pending catalog entry; rolling back to a
savepoint allows a corrected creation with the same name. With orders partitioned
by month, a unique index on `(month, reference)` must also reject duplicate
references inserted earlier in the same transaction into a leaf partition.
A later partition's failure removes the root and any child indexes already
created by that statement. Later uniqueness checks also use own replacements and deletions: deleting an order lets that
transaction reuse its reference before commit, including through a saved index.
An older repeatable read transaction cannot create a unique index that misses
later committed duplicates. The [PostgreSQL 18 index-build implementation](https://github.com/postgres/postgres/blob/REL_18_STABLE/src/backend/access/heap/heapam_handler.c)
is the upstream visibility reference; the differential suite checks actual
PostgreSQL results.

Type renames and schema moves update references only in the type's database,
including pending table, domain, composite and routine definitions. Casts, operators,
operator families and operator classes retain their input and result types too.
Restoring casts from the journal or a checkpoint retains their owning database.
Creating a cast after renaming a domain in the same transaction journals the
domain's current name, so recovery can resolve it. Type usage and ownership
checks resolve the name visible inside that transaction while internal references
retain their bound identity until commit. Converting an identifier to regtype
or calling format_type also uses the current domain or domain-array name.
The reference walk uses each table's existing lifecycle and definition guards.
An unrelated definition can stay open for reading while a type move inspects it.
Waiting for a matching definition holds no shared catalog or definition allocator;
all pending images receive the new type name before their owner can roll back.
No startup pool or memory reservation is added. Complete type commits still
require exclusive storage access; coordinating their publication and enabling
SQL workers remain planned work.
Type comments move within the same database. Composite type renames journal only the name
change, preserving current field references during replay. The
[format guide](durable-format.md#composite-type-name-changes) gives the upgrade
requirement for this record.

**User example:** two databases can both define `app.customer_id`. Renaming that
domain in one database leaves the other database's columns, functions and comments
unchanged, including after rollback or recovery without local caches.

Routine selection permits implicit conversion through a domain's base type,
then checks the declared domain before calling the function. For example,
a positive-integer parameter accepts 7 and rejects -1 with SQLSTATE 23514.
Exact overloads still take priority, including domain operator signatures;
conversions requiring an explicit cast remain rejected. The [PostgreSQL function-resolution rules](https://www.postgresql.org/docs/18/typeconv-func.html)
are the upstream reference; tests compare actual PostgreSQL 18 results.

A catalog type reference (regtype) converts to its numeric object identifier,
integer or bigint. Converting a numeric identifier back to regtype looks up the
current user type name in the selected database. A smallint cast returns SQLSTATE 42846, including for NULL,
unless an explicitly registered cast provides that conversion. Schema-drop
notices retain declared domain and array names in routine and operator signatures.

A custom cast that calls a function marked VOLATILE runs during execution,
rather than while checking constant expressions. For example, a cast that calls
nextval advances the sequence once for each evaluated value. A skipped false
branch or WHERE condition does not advance it. Filtering a SELECT without FROM
happens once before evaluating its outputs or expanding a set-returning function.
Waiting for a nested function does not advance
it again. SQL function result queries use the connection's sequence state.
The pg_typeof function is stable rather than immutable: an immutable cast of
its result stays out of planning. A genuinely constant immutable cast can
still raise a planning error even when WHERE FALSE removes all rows. These
distinctions follow PostgreSQL 18's
[function volatility declarations](https://github.com/postgres/postgres/blob/REL_18_STABLE/src/include/catalog/pg_proc.dat)
and are checked against the running server.

Schema validation uses a current read position for the rows being checked,
including earlier changes by the same transaction. CHECK, NOT NULL, uniqueness,
foreign-key and domain rules, and partition attachment retain their original row
sources. Saved validation rows stream through the merged reader; expression
scratch is reused after each row and capture space after each table. Schema
rewrites fetch original captured bytes rather than consulting mutable version
chains again.

**User example:** a report sees two valid orders, then another session commits
an invalid third order. Adding a CHECK constraint must reject the third order
with `23514`, while the report still sees its original two. A failed check or
savepoint rollback keeps the prior table definition and data.

A foreign key must also accept a new valid parent committed before validation,
including one in a
partitioned parent's leaf. Deleting through that leaf still enforces its foreign
keys. Adding a partition checks the default partition's existing rows before
narrowing its allowed range.

SQL functions declared `STABLE` promise consistent results within a statement. Their queries keep the transaction's ordinary
snapshot in a table CHECK: a function can see an earlier lookup value while the
check examines a new order.
The [PostgreSQL 18 table validation implementation](https://github.com/postgres/postgres/blob/REL_18_STABLE/src/backend/commands/tablecmds.c)
uses a fresh snapshot for these stored-row scans; the reference tests compare
actual PostgreSQL behavior without moving the caller's query snapshot.

ANALYZE captures resident row choices and retains the original heap and saved
files. Saved rows continue streaming through the merged reader. Base and
extended statistics share a current read position for the operation, including
all own changes. Success and errors restore the caller's earlier query snapshot.
The work buffer is reused between target tables, so a database-wide ANALYZE
does not accumulate captured metadata from every table.
Statistics record the data generation examined; table replacement rejects a
captured source with `40001` instead of publishing it onto a reused table slot.

**User example:** an older report still sees two orders after another customer
commits a third. Running `ANALYZE orders` refreshes estimates using the three
current orders, while the report still sees two. Another transaction's pending
changes are excluded; this transaction's earlier changes are included. A failed
statistics expression or a full statement buffer releases readers and permits
retry. Relation estimates survive rollback, while column and extended statistics
retain PostgreSQL's transaction behavior. See the
[PostgreSQL 18 ANALYZE row-selection implementation](https://github.com/postgres/postgres/blob/REL_18_STABLE/src/backend/access/heap/heapam_handler.c).

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

### Numbering changes safely

A log sequence number (LSN) gives each journal record a position. Reserving a
position does not publish a durable commit: the journal still orders staged
records at commit. Each reservation updates a shared, checked counter whose memory is reserved at startup.
Exhaustion reaches the same staging-error handlers as a full journal buffer,
returning `54000` with `WAL LSN space exhausted`. A rejected reservation cannot
claim a journal stage or wrap to an older position. A query that only reads data and has not
acquired a persistent transaction ID (a number that must survive restart) can
finish without a new log position. Transactions that must record an ID or save
changes still need room for their commit. Observing a completed
publication cannot move the counter backward. Recovery can restore an exact
position only with exclusive storage access.

New catalog objects also need unique identities. Exhaustion returns `54000`
before publishing an object or its dependents; the existing catalog diagnostic
is `catalog sequence OID range exhausted`. Lower object-specific object
identifier (OID) limits still apply. View creation reserves its identity before
installing the return rule, so failure preserves an existing replacement target.
Both counter controls are included exactly in the startup memory plan. Their
64-bit representation is not a configurable memory reservation.

**User example:** a table creation that cannot reserve a journal position fails
without leaving the table or its identity sequence behind. An unsuccessful view
replacement keeps the old query and rewrite rule. Existing rows remain intact
when new log positions are exhausted. These guarantees do not enable parallel
SQL execution or replace the remaining durable-commit coordinator.

### Removing tables

Table cleanup and existence changes use a lock belonging to the selected table.
Waiting for that table's retained definition does not reserve unrelated tables
or the shared definition pool. Committed removal marks the table absent before
releasing its rows and dependent metadata. Journal replay retains rows until
checkpoint, as before.

Row cleanup releases the shared version pools and row map after at most 64 map
scan/removal steps, then continues from its saved position. It preserves lookups
for remaining rows between batches. Each removed row releases its pending and
historical version records. A captured row owns copies of its version metadata
and the original bytes, so later pool reuse cannot change its selected values.
The table's cleanup lock is included in the existing startup charge for each
`Table`; cleanup allocates no memory.

**User example:** a storage reader retains an order while that table is removed
and its slot is reused. The retained order still contains its original values,
including the version appropriate to its read position. Another table's cleanup
can finish while removal waits for the first table's retained definition.

Pending row writes capture an admitted table identity before comparing values or
reading saved data. Publication rechecks its database, identity and visibility
under the table-definition guard. Removal during preparation returns `40001`
without recreating rows or consuming pending-version slots, even before the old
table slot is reused. A new capture after rollback can proceed normally. Waiting for one table
definition releases the shared snapshot registry; retries retain the original
table identity instead of adopting a replacement.

These are storage-level guarantees: SQL execution is still serial and its
transaction-level relation locks still apply. Transactional table creation takes
a separate name-reservation guard and the selected slot's cleanup lock. A slot undergoing cleanup cannot be reused;
when every slot is live or busy, creation returns `54000` with the table limit.
Retry after cleanup releases a slot can succeed. Initialization of an empty
slot resets prior rows, saved-file references, statistics, sequences, permissions,
and index-cache bindings before returning. Failed cache initialization releases
its claims and allows retry with the same name. The name-reservation guard
covers empty-slot initialization, without object I/O, and does not guard ordinary
row reads or committed row publication. Committed journal-replay creation still
requires exclusive storage access. Its startup memory plan includes the exact
size of the name-reservation mutex.

**User example:** two connections creating the same table cannot claim separate
slots for its name. Each connection sees only its own uncommitted creation.
A read and an update of an existing order can progress while table creation
waits for its reservation guard; this storage-level test does not enable
parallel SQL execution.

### Changing rewrite rules

A rewrite rule changes what a command does before it runs. For example, a rule
can redirect an insert into an audit table. Changing that rule takes the target
table or view's exclusive relation lock, shared with ordinary reads and writes.
Unrelated relations remain usable.

For example, connection A can run `BEGIN; CREATE RULE ignore_insert AS ON INSERT
TO orders DO INSTEAD NOTHING`. Until A commits or rolls back, connection B's
`SET lock_timeout = '50ms'; SELECT * FROM orders` returns `55P03`. A query against
an unrelated table can continue. A failed or rolled-back rule creation does not
publish the table's rule-history flag; committed history remains set after the
last rule is dropped. Savepoint rollback restores both the history and locks.
`pg_rules` shows user rules; a view's internal `_RETURN` rule remains visible
through `pg_rewrite`.
After a view is renamed inside a transaction, rule listings use its new name.
View changes and removal use those names too. The same names are recorded for
restart; rollback restores the old names.

Table and view slots have distinct lock identities. The lock registry reserves
space at startup for each transaction's tables and views; exhaustion returns
`54000` naming the registry. Prepared transactions retain view locks through
restart and recovery without local caches. Saved commit and rollback decisions
release the transaction's private rows and catalog entries before replay, so
recovery cannot wait on its own unfinished table or sequence creation. Live
resolution, startup recovery, and savepoint rollback share catalog undo.
A transaction listed as prepared in a checkpoint may have committed or rolled
back later. Recovery verifies its original saved batch and identities before
applying later decisions; a missing or mismatched batch still rejects startup.
`pg_locks` reports the actual relation
and database identities; a recovered prepared lock has no client process ID.
Client libraries may ask which columns a write will return before running it.
Those descriptions and catalog checks of writable views only read metadata;
they do not reserve execution locks for an idle connection's last transaction.
Actual writes lock their target before selecting rules or rewriting a view.
Statements still execute serially; these protections
are prerequisites for the planned execution workers.

### Reading index caches during replacement

A selected in-memory index cache owns its storage. Rebuilding or dropping a table
cannot reuse that cache slot until its last reader releases it. A replacement
binding list and its count publish together after population; failed builds
release their claims and publish no partial list. A cache whose row map fills is
marked incomplete, so it cannot supply an authoritative negative lookup.

Cache owners and their buffers are reserved at startup. Retained readers count
against `max_value_indexes`; a full claim pool returns `54000`. For example, a
replacement that runs out of slots can be retried after an older lookup releases
its cache. Allocation controls release before clearing a claimed cache.

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

Appends return owned row images and release initialization locks before metadata
publication. SQL writes, foreign-key cascades, large-object pages and prepared
replay retain those images through preparation and retries. Deletes carry no
row image. Raw-location pending-write adapters are confined to test fixtures. If compaction
replaces the heap buffer between preparation and publication, publication
retries and copies the retained input into the current startup-sized buffer;
insufficient space returns `54000` without publishing the row. For example, an
order update waiting to publish cannot accidentally use another row at the old
address after compaction. A dropped or replaced table still returns `40001`
before a relocation retry. Update comparisons retain the committed row and
the new input independently, so they may read from different heap buffers.
Compaction checks its complete relocation set before changing bytes or handles.
Readers reject
reused table identities and stale heap locations. Heap addresses and relocation
generations describe caches, not durable row identity. Retained byte copies
use statement memory.

Heap relocation reserves the existing spare buffer, captures resident locations,
then sorts and copies privately in bounded turns. Only capture and the final
remap own the shared row-version metadata. A write prepared before capture
moves its unpublished input into the append tail; writes after capture already
use that tail. Relocation copies later appends before publishing. If the tail
advances again, publication requests another copy turn without changing row
locations. Current rows, history, pending versions and aliases are remapped by
their old location rather than reusable allocator slots.

For example, one connection can retain an old order while another updates a
different order during copying. Both still read their own selected bytes after
the buffer switch. Table retirement clears its rows through version ownership;
a replaced slot's new rows survive publication. A held table definition does
not keep copying or unrelated publication waiting.

The two heap buffers and coordination controls are charged at startup. The
existing relocation scratch now stores typed source/destination entries.
Exhaustion returns 54000: "heap compaction scratch overflow", "row heap relocation
buffers are retained by readers", or "row heap relocation is already active".
Cancellation releases the private buffer and capture boundary. Exclusive
callers can still compact an unretained active buffer in place when an older
reader retains the other buffer. Shared callers always use the private copy
path.

Ordinary automatic checkpoint cleanup copies at most 64 KiB of heap bytes per
cleanup step. The server advances one step between messages or while idle,
including while an unrelated object download is waiting. For example, a slow
invoice download does not prevent the server from reclaiming unused row memory.
After local cleanup finishes, the reactor waits for download readiness rather
than repeatedly polling maintenance that needs that download to finish. Each SQL
statement also advances at most one step before executing; it does not drain
the complete copy. For
example, a large cleanup can remain pending while a client reads an order,
updates another order, and reads the result. Later appends join the copied tail.
An explicit CHECKPOINT waits for all cleanup phases and preserves exclusive
in-place reclamation when the active buffer has no retained readers. At the
existing 85% critical heap threshold, automatic checkpointing also finishes
publication and cleanup before dispatch resumes, preserving headroom for the
next write. This pressure path can still delay connections.

The startup relocation workspace owns its scratch and private copy controls
across turns. Moving the engine does not invalidate them; cancellation or a copy
error releases the reservation. Using a workspace with a different storage
instance returns XX000, "heap relocation belongs to another storage instance",
without publishing either instance. Journal cleanup runs once for the published
prefix; writes after it keep their journal suffix for recovery. Heap capture,
sorting, final remapping, eviction, and other maintenance still run within server
turns. Checkpoint publication, merges, and temporary spilling still wait for
pending durable-block downloads. Fixed execution workers and independent storage
publication remain unfinished.

For example, while one connection downloads an uncached invoice, another can
update a cached order and receive its durable commit acknowledgement. INSERT,
UPDATE, DELETE, COPY, trigger execution and commit use the existing separate
block client for any reads that must finish without restarting the operation.
They preserve the suspended reader's socket, response bytes and error, including
across nested calls and SQL errors. The reader keeps its original statement
snapshot. A write that needs an uncached block still waits synchronously; it can
issue another GET even if a reader is already downloading that block. SQL
execution remains serial, and this does not enable independent checkpoint
publication or worker threads.

Object-read byte counters include each successfully completed HTTP response
body, including its block header, even if block verification rejects it.
Reusing a completed response does not count it twice. The HTTP client discards
a connection when the server advertises that it will close it, while retaining
the completed response for its owner.

The object-request reservation includes every configured read client, the
separate atomic block and metadata clients, read-slot controls, and the 256 KiB
block scratch buffer. Checkpoint writer buffers and workspace arrays are charged
when constructed too. For example, increasing `object_store_get_slots` reserves
another complete request/response buffer before accepting connections; it cannot
borrow memory later from the row heap. Insufficient startup memory names the
component that could not be reserved.

Commit-history cleanup keeps the entry selected by the published chain, even
when a newer filename belongs to an upload that failed before publication. For
example, a failed order upload cannot cause cleanup to delete the history used
by a confirmed order. Boundary discovery reads at most one commit descriptor per
turn. Deletion count and staging memory keep their configured limits; listing
the namespace can still span many pages. Retries
retain their cursor after errors, and later commits can extend the chain.
Pending journal uploads retain their bytes through local cleanup, including
when the snapshot already covers them. With no pending block download, the
publication retry runs before local cleanup continues.

Eviction requires an installed object's recorded boundary to cover the row's
commit position. A later update stays in the heap; history newer than that
boundary stays resident too. Recovered roots have no eviction boundary and do
not justify discarding newer heap bytes. A temporary spill records the table
generation it copied, because temporary commits may share a zero journal
position. Only that exact table generation can evict to the temporary snapshot;
a later spill names its own object. These kinds of boundary are distinct states in the
startup-sized root metadata. For example, if a checkpoint saved an order before its price
changed, cleanup keeps the new price in memory until another checkpoint saves
it; it never redirects that row to the older object.

Table rewrites retain each transformed image until validation, journaling and
publication finish. The owner array uses the existing statement arena; its
scoped destructor releases consumed and unconsumed images after success, an
error or panic. Per-row scratch rewinds cannot reclaim it. Insufficient owner
storage returns SQLSTATE 54000, "ALTER rewrite retained inputs exceed the
statement arena"; increasing the startup statement-memory reservation increases
this allowance. It does not create a growing pool. For example, a rejected
type conversion leaves the original table readable and allows a later retry
to reclaim the heap buffers.

Selected enum definitions and label iterators retain the label image they read.
A label image is one complete saved list of labels.
Changing or dropping the type, rolling back a later change, reusing its catalog
slot, replaying a definition, or retiring a database cannot replace those
selected labels. Iterators release their image at the end or on drop; cloned
definitions release it when dropped. The label buffers remain alive after Storage is
removed while a selected reader still owns it. For example, a reader that
selected the status labels `new` and `paid` keeps those labels while an ALTER
adds `shipped`; another lookup sees the new definition.

Committed definitions, pending changes and retained readers share the existing
startup label buffers. A separate image remains reserved for replay.
Image counters and controls are charged at startup. Exhaustion returns 54000,
"enum member-image pool is exhausted", without changing the published
definition. Release old readers or increase the startup catalog reservations
before retrying. Replay can reuse an unretained image in place; it reserves a
different image when readers retain the old one. Type commits remain exclusive.

Transaction-wide catalog and durable publication, concurrent cache/object I/O
and maintenance, remaining table rewrites, and execution workers are still open.

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
