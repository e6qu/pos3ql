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

### Connections keep the same user

A connection belongs to the account that logged in, even if an administrator
renames that account. `current_user`, `session_user`, `RESET ROLE` and
`RESET SESSION AUTHORIZATION` follow that account. Role changes saved in a
transaction or savepoint retain the same account through rollback.

For example, connect as `orders_app`, then run these commands as an administrator
in another connection:

```sql
ALTER ROLE orders_app RENAME TO orders_writer;
CREATE ROLE orders_app LOGIN;
```

The existing connection reports `orders_writer` and creates objects owned by
that account. The new `orders_app` account has its own permissions and connection
limit. If the original account is later deleted, its existing connections can
still run `SELECT 1`, but `current_user` and `session_user` return `42704`.
Creating objects returns `42501`; recreating either name does not restore the
deleted account's permissions. A replacement account cannot inspect or cancel
the old connection's queries.

Role identifiers also survive journal and checkpoint recovery. Renaming the
owner of a prepared transaction changes the displayed owner name. Deleting
that owner leaves the transaction available to superusers; a replacement
account cannot commit or roll it back. Reusing a catalog slot does not reuse
the deleted account's identifier.

Deleting a role waits for another transaction's unfinished ownership or permission
changes. Renaming the account can proceed while its table creation is unfinished.
Metadata changes retain a shared account lock until commit or rollback; deleting
that account needs an exclusive lock. Creating objects for the same account can
overlap. Savepoint rollback releases locks acquired after that savepoint, and
prepared transactions keep their account locks through recovery. `pg_locks`
reports these as object locks using the account's durable identifier.
For example, if `orders_app` has created a table inside
`BEGIN`, an administrator's `DROP ROLE orders_app` waits for that transaction.
After commit, the table prevents deletion; after rollback, deletion can proceed
if nothing else depends on the account. Dependencies include objects in other
databases, publications, subscriptions, user mappings and grants made by the
account. Recovery preserves role catalog positions as well as identifiers.

Memory for connection identities is reserved at startup. The identities distinguish catalog slot reuse.
Creation without a valid acting account fails before allocating identifiers or
changing catalogs. Recovery supplies an explicit owner instead of substituting
the bootstrap administrator for an absent user.

## Runtime ownership

This section explains the recent concurrency preparation. “Ownership” means
keeping data and buffers valid until the operation using them has finished.
SQL statements still use one CPU worker.

A mutex protects one shared value from simultaneous changes. Core mutexes
keep their native state inline, including on macOS using
[Apple's native lock](https://developer.apple.com/documentation/os/os_unfair_lock).
First use, replacement, movement, contention and recovery after a panic are
checked with allocation guards and an interpreter that detects invalid memory access.
Read/write locks allow several readers together or one writer.

**User example:** a connection's first query and a newly created materialized
view can use their lock state without requesting additional heap memory.

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
| Catalogs and table definitions | Readers retain selected definitions and identities; tables have separate cleanup owners. Table and routine submission validate type lifetimes under installation owners. Typed tables and saved queries retain source lifetimes through recovery and database cloning. | Consistent reader capture across transactions and concurrent maintenance |
| Type definitions | Queries retain selected domain, enum and composite definitions. Table reads select nested type dependencies before storage waits; nested statements select their own visible versions. | Consistent type and table versions across later statement lookups and maintenance |
| Sequence positions | Per-table synchronization, checked arithmetic, coherent journal/checkpoint images, and acknowledgement of unchanged staged positions | Preserve these rules through engine-wide concurrent publication |
| Row versions | Pending writes, rollback, committed publication, row maps, and visibility caches use guarded state. Point metadata reads own bounded copies of their chains and byte sources, allowing unrelated publication and later selection after removal or shutdown | Safe movement and reclamation while readers remain active |
| Planner statistics | Readers copy their visible estimates; private versions and saved-state markers use guarded cells and fixed pools | Integrate these captures with concurrent engine publication |
| Row bytes | Selected rows own their startup heap allocation; reclamation can copy live rows into an unretained buffer | Integrate relocation with concurrent table publication and maintenance scheduling |
| Read positions | Statement admission and the last fully installed commit share an owner; existing statement and transaction snapshots retain history. Replication export validity and import protection share that guard, with separate fixed capacities | Extend admission to coherent later catalog lookups and every table lifecycle path |
| Deferred reads | Scans and rewrites keep their original byte sources; UPDATE, DELETE, and MERGE copy selected values into fixed statement memory | Finish coherent table lifecycle changes |

### How long reads keep data alive

Executing SQL and large-object fast-path statements register an owned snapshot
lease: a reservation recording the read position they must preserve. Each
query workspace reserves 1,024 slots at startup. Registration happens under
the same synchronization as row publication, so it cannot miss an intervening
commit. Nested statements do not replace a transaction's snapshot. Completion,
errors, and transaction end release retention, wake schema waiters, and
schedule pruning.

**User example:** a transaction updates an order and its invoice from 10 to 20.
A report already reading the old versions can finish while the commit installs
both changes. A new report reaching this installation step yields through the
existing lock-wait mechanism and resumes after both values are installed. It
cannot select the position of an unfinished commit. Choosing that position and
retaining its history happen under the same owner, so cleanup cannot slip
between them. Journal position reservations do not advance the visible position.

Installation starts after the object-store durability wait and ends after the
ordinary or prepared transaction's changes and outcome are installed. It does
not wait for existing reports to finish. An abandoned partial installation wakes
waiting connections but rejects new admission with `08007` until recovery;
it does not expose the partial result. Lock-only mode uses the same admission
boundary without reserving row-history slots. These controls are charged at
startup, with no new pool or configuration limit. SQL still uses one CPU worker;
consistent later metadata capture and remaining synchronous storage reads are
separate [remaining work](../PLAN.md).

A large-object protocol call waiting for admission keeps its original statement
clock and timeout. It creates no object before admission. An expired wait
returns `57014` and can be rolled back; a successful retry performs the call once.

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
same table remain visible to authoritative walks. Lookup buckets contain row
slot positions; a separate fixed array stores each row's state once. Deleting a
lookup key never moves another row slot. The lookup index and row array are
charged at startup, including the independently sized large-object page table.

History cleanup retains a fixed cursor instead of copying every table's row
identities. An ordinary server turn visits at most 64 stable slots; an empty or
busy table consumes one work unit. Work continues between messages, while idle,
and during an unrelated download, including without object storage enabled.
Each batch rechecks current readers under registry, saved-file, version and row
owners. A busy owner is skipped without retaining earlier guards. The server
uses its existing one-second retry cadence for unfinished contention and keeps
storage readiness unchanged. Lookup shifts, removal and table clearing cannot
move unvisited row cells; new releases remain queued independently of the pass.
The slot quota counts metadata visits, not elapsed time: a row's bounded version
chain and lookup-index deletion are part of that slot's work.

Starting a statement does no history sweep. Normal commit and rollback release
retention immediately and schedule maintenance. Callers that require
complete reclamation select synchronous completion explicitly. Row publication
sets a conservative pressure signal; a completed pass clears it only if no
publisher changed an earlier row during the pass. Checking pressure needs no
row scan. Before a row write, readers at or after its current committed version
do not keep an obsolete earlier boundary, avoiding a false one-version capacity
error while cleanup is pending. Older reports still retain the versions they need.

**User example:** ending a report queues cleanup instead of walking a large
orders table before the next query can start. Later turns release small batches
while another customer reads or updates an order. A report already using an old
total keeps those bytes even after its metadata is reclaimed.

Pending table definitions retain their table and one reserved version slot.
A reader releases the pool's allocation control before returning the definition;
another table can publish, roll back, or retire its own versions while that reader
keeps its selection. Controls and version capacity are charged at startup.
A full pool or version history returns `54000` before changing the published head.

An edit remembers which table and definition it read. Replacing that table or
changing its definition while the edit waits returns `40001`; inserting a row
alone does not invalidate the definition. Capturing the definition and its
version uses one read owner. Waiting releases earlier owners and checks the
original selection again. Type changes invalidate affected selections using
the type edit's reserved version; commit does not request another version.

Statements retain the table definition used to prepare their values, including
COPY input, partition routing, foreign-key actions and replication tuples. Row
installation takes that original selection instead of looking up a fresh layout.
A rewrite receives its new selection from the operation that installs the new
definition. Row preparation copies the column types and count with that version. A definition change before the prepared write is installed returns
`40001`, including after a wait or heap relocation. Another table's definition
change does not invalidate that write. Rollback records retain their original
storage and table lifetime; matching counters in another server's storage cannot
remove its pending row. Undo still works after a column change on the same table. Row installation
and recording its rollback happen in one transaction operation. A full row undo
log returns `54000` and removes only the write it could not record; a selection
from another transaction returns `40001` before installation.

**User example:** add an order total, update it, then roll back a later type
change and delete. The earlier total remains. Committing a subsequent type
change keeps that value after recovery with an empty local directory. Storage
regressions separately force a definition change between row preparation and
installation; they check that it fails before rows or write counts change.

An invoice import can remain open while an order column is physically rewritten.
The rewrite discards the earlier layout from new row lookups; an older repeatable-read
transaction that first opens that table after commit sees it as empty, as described
in [PostgreSQL 18's rewrite caveat](https://www.postgresql.org/docs/18/mvcc-caveats.html).
A byte source selected before replacement still reads its original heap or saved
file. Existing table locks protect SQL reports already reading the affected table.

Finishing a rewrite schedules old history for cleanup instead of walking every
row during commit. Fresh lookups exclude that history immediately; each background
cleanup turn visits at most 64 row slots and releases its owners between turns.
Already selected row bytes remain readable after cleanup. A metadata lookup saved
before the physical layout changed returns `40001` rather than decoding replacement
bytes using the old column types. Renames and unrelated row changes do not invalidate
that lookup. Retained scans also reject newly supplied row images from another
physical layout after commit, even when their bytes share a heap allocation.
These checks use table publication generations, so temporary changes
sharing a journal position cannot redirect an old selection to new bytes.

**User example:** deleting most orders and then changing their ID from `integer`
to `bigint` does not add a second whole-table history sweep to commit. An invoice
import keeps progressing while old order history is reclaimed in small steps.

Rollback records identify the table's lifetime and the installed definition.
An old record cannot undo a newer edit or change a replacement table. SQL
installs the edit and records its undo in one transaction operation, before
journal staging continues. A full rollback log returns `54000` and removes only
the change it could not record. Column mappings compose back to the committed
layout; table, index and comment names follow the selected shape. Rule history
and pending shapes check transaction owners under the same definition guard.
Comment text publication preserves name changes owned by another transaction.

Table-definition commit and journal replay use the same installer. It owns the
affected table definitions, index bindings, comments, sequences and saved-query
references before changing them. A parent-table rename updates foreign keys in
both committed definitions and every private version, including versions submitted
during preparation. Owned sequences follow the table and column through pending
sequence changes too. Waiting releases the earlier table and catalog owners;
unrelated row commits can progress. A rolled-back or replaced source is checked
again after the wait. An exhausted publication counter returns `54000` before
any index binding or catalog entry changes.
The parent's name view is published before any dependent definition is released.
A foreign-key lookup can wait for the parent; it cannot miss the moved parent
because a child with an earlier internal slot became readable first.

Renames and schema moves share the same SQL dependency preparation. Inside the
transaction, invoice foreign keys use the moved table and an owned sequence is
already available in its new schema. Rolling back restores the earlier names
without restoring sequence numbers already issued.

**User example:** rename `orders`, move it to another schema, and rename its
amount column in one transaction. Its indexes, comments, owned sequence and
invoice foreign keys follow the committed names, including after recovery with
an empty local directory. A private invoice-table edit cannot later restore the
old parent name. Several SQL workers still require consistent later catalog
lookups within each statement; see the [remaining work](../PLAN.md).

**User example:** create an order table, save a transaction position, then drop
and recreate that table with different rows. Rolling back to the saved position
restores the original table and rows. Earlier column changes and a type rename
also survive commit and recovery with an empty local data directory. The
[shared PostgreSQL comparison](https://github.com/e6qu/pos3ql/blob/458738f54100c19ff0ed2ac9c7ad92ac57829df7/tests/external/differential_exact/12_table_definition_undo.sql)
exercises these cases; storage tests also force internal table-number reuse.

Table reads and scalar function contracts capture the complete type chain before
waiting for row data or running the function body. For
example, a shipment column can contain a composite whose status field uses a
domain over an enum. The query saves that composite layout, the domain's rules
and the enum's labels together with their source lifetimes. Resuming the same
statement uses its selected definitions. A later nested statement selects its
own visible definitions. Identical images of the same definition revision are shared within the outer statement;
repeating an unchanged nested query does not consume another image.

Domain checks, composite input/output, enum value resolution and comparisons use
retained readers. Comparisons receive the actual transaction explicitly, and
enum readers retain which new labels are safe to use before commit. Rollback or
source retirement cannot change a selected image. A retained source slot cannot
be reused or assigned a different creation identity until its readers release it.
The readers and their buffers can outlive the storage owner.

Type edits capture the current definition and its revision together. Storage
checks the source lifetime, database, transaction and revision before accepting
the edit. A replaced source returns `42704`; a changed definition returns
`40001`. An edit targeting the wrong type kind returns `42804`. Another
transaction's pending edit or deletion waits on its owner. The type catalog
lock is released before the statement waits. A transaction uses its own pending
name when dropping a renamed type.

Creation and deletion records retain their source identity, transaction and
change revision. Commit validates type ownership and dependent table publication
counters before durability; publication and rollback use the captured records.
Counter exhaustion returns `54000` before a type move changes its source or dependents.
A superseded record cannot overwrite a
newer edit, revive a replacement type, remove its comment or release its enum
labels. Domain, enum and composite revision counters reserve 16 bytes per type
slot plus their controls at startup. These counters are local ownership checks
and do not advance the durable catalog identity allocator. Durable type identities
are unchanged. Subscription creation records now save their source identity
explicitly; see the [journal format](durable-format.md).

**User example:** `BEGIN; ALTER DOMAIN price RENAME TO order_price; DROP DOMAIN
order_price; ROLLBACK;` leaves the original `price` domain available. Rolling
back a savepoint after a type rename and deletion restores both the earlier
name and definition. A later edit in the same transaction can still commit.
Checks and cascading deletion follow captured domain parents and columns across
pending name changes. For example, renaming a parent domain before adding a
check still validates rows declared with its child domain. Moving a domain to
another schema before deleting the old schema preserves outside table columns
that use the moved domain.

Domain, enum and composite creation use shared storage ownership. A table, view
or materialized view also reserves its name for the type describing its rows.
A named type cannot use that name in the same schema. For example,
after `CREATE TABLE public.orders (id integer)`, `CREATE DOMAIN public.orders AS integer`
returns `42710`; renaming or moving a type cannot create that collision either.
The checks and definition installation share catalog ownership, including when
two creators race. A wait releases partially acquired catalogs. Table name
checks use the small name views, so an unrelated definition reader need not
finish first. Removing a table clears its type comments before allowing a new
type to take the name.
Renaming to the existing name returns PostgreSQL's duplicate error; moving to
the current schema succeeds without changing the object.

Each type kind reserves `max_<kind> * (max_catalog_versions_per_object + 1) *
query_workspace_slots` definition images at startup (`max_domains`, `max_enums`
and `max_composites`). Statement selection maps and ownership controls are also
charged at startup. No image allocation grows the runtime heap. A full pool
returns `54000` naming the domain, enum or composite definition image pool and
its capacity; releasing retained readers permits retry. Graph traversal uses
temporary statement-arena space and reports `54000` if it cannot fit. This
protection does not enable parallel SQL execution: engine commit, remaining
retirement paths and metadata preparation stay in [PLAN.md](../PLAN.md).

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
Creation also checks both committed and pending names through this view. If a
matching table slot is reused during a wait, the conflict check selects it again.

**User example:** checking an order's customer reference can find the customers
table while an unrelated table definition is being changed. Looking for a table
that does not exist, or creating a table with a different name, also avoids
waiting for unrelated definitions. SQL execution
is still serial; this removes a storage-level dependency before workers are added.
Readers still need one consistent view of a transaction's definitions and rows
before SQL workers can run together; see [PLAN.md](../PLAN.md).

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

SQL identifiers for tables, views, sequences and indexes follow the object's
lifetime. Renaming an object keeps its identifier; dropping it and creating
another object gives the replacement a fresh identifier, even when storage
reuses the same slot. Tables and views also keep separate identifiers for their
row types and arrays. JSON-to-row conversion accepts table and view row types
and enforces their declared field limits. Journal and checkpoint recovery
preserve these values.

**User example:** an application can save a table's identifier before replacing
it. The old value cannot give permission to read the replacement:

```sql
CREATE TABLE orders(id integer);
CREATE TABLE saved_table_id AS SELECT 'orders'::regclass::oid AS oid;
DROP TABLE orders;
CREATE TABLE orders(id integer);
SELECT has_table_privilege(oid, 'SELECT') IS NULL FROM saved_table_id;
-- true
```

An index created by a constraint keeps its identifier when an earlier
constraint is removed, its own constraint is renamed, or a column moves.
`ADD CONSTRAINT ... USING INDEX` keeps the existing index's identifier.
Drop and recreate reserve a fresh one. Committing an index drop or rolling back
its creation also releases its statistics entry; repeated replacements reuse
the fixed statistics pool.
Savepoints keep counters for indexes they can still restore.
Counters remain separate across databases, including cloned databases with the
same numeric identifiers. Identifier exhaustion returns `54000`
without installing a replacement; existing data and recovery remain usable.
The [format guide](durable-format.md) specifies the bounds. Remaining identifier
families stay in [PLAN.md](../PLAN.md).

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

Sorted scans retain one source for their selected heap
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

Domains, enums and named composite types share one namespace in each schema.
Creation, renaming, schema moves and journal replay use the same name check.
A conflicting visible name returns `42710`; another transaction's pending
creation, rename or drop waits through the existing transaction wait path.
A pending rename reserves both names for other transactions. Its owner can
reuse the old name, and rollback releases the proposed name.
For example, after `CREATE TYPE order_code AS (value integer)`, renaming a
domain to `order_code` fails instead of creating two types with that name.

Type renames and schema moves update references only in the type's database,
including pending table, domain, composite and routine definitions. Casts, operators,
operator families and operator classes retain their input and result types too.
Restoring casts from the journal or a checkpoint retains their owning database.
Creating a cast after renaming a domain in the same transaction journals the
domain's current name, so recovery can resolve it. Type usage and ownership
checks resolve the name visible inside that transaction while internal references
retain their bound identity until commit. Converting an identifier to regtype
or calling format_type also uses the current domain or domain-array name.
Publication acquires affected table definitions and the dependent catalogs before
changing the source type. If any owner is busy, it releases every earlier owner,
waits for that one, then selects the current references again. An unrelated
definition can stay open for reading. Waiting holds no table lifecycle, shared
catalog or definition allocator. All pending images receive the new type name
before their owner can roll back.
Table and type publication share one ownership workspace, reserved at startup
for every configured table plus the internal large-object relation: one optional table-owner record per slot,
alignment padding and its mutex/arena controls. It never grows at runtime.
Finishing a type deletion also releases the type catalog while waiting for
comments or enum-label ownership, then rechecks the original type lifetime and
transaction. An unrelated type can be created during that wait. Rolling back
the deletion prevents the waiting completion from removing the restored type;
retained enum labels remain readable after a completed deletion.
Saved type references carry the source catalog, database, type slot and creation
stamp. A definition submitted after a rename receives the source's current name.
A reference to a dropped type fails with `42704`, even if another type has reused
its name or slot. A captured reference with a different representation fails with
`42804`. These checks use startup memory and do not allocate during execution.
Routine arguments, parameters, result columns and aggregate state/results share
one field walk for binding and renames. Checkpoint declarations bind after their
type catalogs exist. A database template copy explicitly binds copied references
to the new database's types.

Views, rewrite rules, materialized views and row-security policies install their
definitions and saved type dependencies under the same catalog locks. A table's
definition and lifetime stay protected through installation. Waiting releases
earlier locks, then rechecks the selected table and source types. Reusing the
target table slot fails with `40001`; reusing a source type fails with `42704`.
Dependency capacity and policy roles are validated before changing live entries.
Rule renames and enable-mode edits apply to the current definition and journal
that installed definition. They preserve intervening edits and update comments
and saved dependencies together. Recovery follows the same validation boundary.

**User example:** a report view bound to `app.customer_id` keeps referring to
that domain after it is renamed. Dropping it and creating a different domain
with the old name cannot redirect a waiting view definition to the replacement.
A failed policy edit keeps the previous expression and allowed roles intact.

Domain, enum and composite commits now use shared storage access. A name move
rechecks its source and edit revision after waiting, and includes tables created
or changed while it was acquiring locks. Source installation and dependent
updates share those locks; exhausted publication counters leave everything intact.
Saved journal references retain their source lifetime even when a rename commits
before an older dependent definition. Checkpoints preserve the same identities
and resolve type definitions before their dependents. Domain, type and schema
changes wait on their affected objects rather than every retained snapshot.
For example, importing invoices can continue while an unrelated customer domain
is renamed. Schema cascades lock affected tables and views before changing them.
Transaction-wide publication and SQL CPU workers remain [planned work](../PLAN.md).

Adding or validating a domain rule takes PostgreSQL-compatible SHARE locks on
tables whose values must be checked. Readers can continue; unfinished writers
must finish first, and validation then selects current committed rows. Unchanged
NOT NULL settings do not scan values. VALIDATE CONSTRAINT checks existing values
again even when the rule was already marked valid.

PostgreSQL rejects validation when a domain is stored inside arrays or composite
values. For example, a column declared as `customer_id[]` makes immediately
validating a new rule on `customer_id` fail with `0A000`; the old rules remain
intact. `ADD CONSTRAINT ... NOT VALID` can install a rule for future values
without scanning old ones. A domain
whose value is an entire array validates that whole array. Unstored type
definitions, views and foreign-table columns do not create local values to scan.
The dependency walk uses the connection's SQL arena, reserved by `sql_arena_bytes`
at startup, and returns `54000`, "domain dependency graph exceeds the statement
arena", if that space fills. Temporary dependency links and the work queue are
released before scanning rows. Table sources reuse the startup-reserved definition
images; their pool reports `54000` if exhausted.
If the spilled-row scan runs out of scratch space, its error reports the supplied
arena's name and capacity; this can be the connection's SQL arena or a query
workspace, so the scan does not prescribe a different reservation's setting.
These rules follow
[PostgreSQL 18's domain validation](https://github.com/postgres/postgres/blob/REL_18_STABLE/src/backend/commands/typecmds.c#L2870).
Type comments move within the same database. Composite type renames journal only the name
change, preserving current field references during replay. The
[format guide](durable-format.md#composite-type-name-changes) gives the upgrade
requirement for this record.

**User example:** two databases can both define `app.customer_id`. Renaming that
domain in one database leaves the other database's columns, functions and comments
unchanged, including after rollback or recovery without local caches.
If a report retains an affected table definition, publication waits with the old
type name intact. Once that reader releases it, the type and its dependent
definitions change together; an earlier table in the update does not remain
locked while publication waits for a later one. This tests the storage ownership
boundary; SQL statements still execute on one CPU worker.

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
Commands that wait for a lock also retain their completed sequence calls. For
example, if a schema-drop event trigger inserts an audit row with a serial ID,
waiting for a report to finish does not consume another ID on each attempt.
Calls to `nextval`, `currval`, `lastval` and `setval` retain their results in the
existing statement arena; automatic serial values use the same path. Exhaustion
returns `54000`, "volatile sequence replay exceeds statement memory", before
making an unrecorded sequence change. Saved handles reject a reset or replacement
arena, and a changed source or call order returns `40001`. A private sequence
recreated by rollback must reproduce its saved result before execution continues.
Retries preserve previously reserved sequence values and the connection's prior
last-sequence source.
`setval('orders_id_seq', 90, false)` makes the next generated ID 90 while leaving
this connection's previous `currval` unchanged. `lastval` follows the last sequence
used by `nextval`, checks access to that source and rejects a dropped or replaced
source with `55000`. These rules follow
[PostgreSQL's sequence functions](https://www.postgresql.org/docs/18/functions-sequence.html).
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

### Changing schema names

**User example:** inside `BEGIN`, rename the schema `orders` to `sales`. That connection
can immediately use `sales.order_view`; other connections keep using
`orders.order_view` until commit. A rollback or savepoint restores the earlier
name and dependent definitions. Prepared transactions retain the private change
after loss of local disk, until explicitly committed or rolled back.

The rename updates bound table, type, sequence, view, rule, routine, index and
policy references together. It distinguishes schema qualifiers from query
aliases and preserves quoted names, including commas and embedded quotes.
SQL routine bodies bound at creation follow the change. A string body such as
`AS 'SELECT * FROM orders.items'` keeps its text and can fail when called after
the rename, as in PostgreSQL.

Stored object references follow the rename too. For example, a saved view that
calls `nextval('orders.ticket')` keeps using that sequence after `orders` becomes
`sales`. PostgreSQL's object-reference casts, such as
`'orders.items'::regclass`, follow the object. Ordinary text stays unchanged;
`nextval('orders.ticket'::text)` resolves its text when called and can fail after
the rename. Quoted names preserve their Unicode characters, dots and quotes.
Object-name strings follow PostgreSQL lookup rules: `to_regclass('bad..name')`
returns `NULL`, while `'bad..name'::regclass` reports invalid name syntax.
`'ticket-1'::regclass` can name a table created as `"ticket-1"`;
`parse_ident('ticket-1')` still rejects it as an unquoted SQL identifier.
Looking up `private_schema.orders` requires `USAGE` on that schema.
A missing relation returns `NULL` from `to_regclass`; a permission error remains
an error. Schema lookup does not itself require permission to read the table.
Schemas without `USAGE` are skipped in `search_path`, as in PostgreSQL;
`SET search_path TO private_schema` does not grant access. Privilege checks accept
quoted names too: `has_table_privilege('orders."line.items"','SELECT')` checks
that table, rather than splitting its quoted name at the dot.
Sequence calls also accept `regclass`, `oid` and integer identifiers, following
[PostgreSQL 18.6's input casts](https://github.com/postgres/postgres/blob/REL_18_6/src/include/catalog/pg_cast.dat#L95-L102).
A numeric sequence identifier bypasses a schema-name lookup; it still requires
permission to use the sequence. An administrator can look up
`SELECT 'orders.ticket'::regclass::oid`, then give that number and sequence
`USAGE` to a role. The role can pass the number to `nextval` without schema
`USAGE`.
Missing sequence names return `42P01`; a nonexistent numeric identifier returns
`XX000`, matching PostgreSQL's failure to open that relation.

A schema keeps its object identifier through renames and recovery. Renaming
`public` to `archive`, then creating another `public`, gives the replacement a
different identifier and ordinary ownership; `archive` retains identifier 2200
and its database-owner role until explicitly reassigned. Restart also works
when no replacement `public` exists. See the exact identifier limits and saved
records in the [durable format guide](durable-format.md).

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
or the shared definition pool. Removal keeps the table name reserved until
comment, permission and extension cleanup can complete under their owners.
Unrelated type creation can proceed while that cleanup waits. The table becomes
absent before its rows and remaining dependents are released. Journal replay
retains rows until checkpoint.

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

### Changing sequences and routines

Sequence edits, function bodies and names retain the object and revision selected
before a wait. Savepoint rollback and a full transaction rollback restore the
corresponding definitions and comments. Old undo cannot alter an object created
later in the same slot. A full undo log rejects the command with `54000` and
releases the staged change. The undo images occupy the startup-reserved log.

```sql
CREATE SEQUENCE order_numbers START 10;
SELECT nextval('order_numbers'); -- 10
BEGIN;
ALTER SEQUENCE order_numbers RESTART WITH 50;
SELECT nextval('order_numbers'); -- 50
ROLLBACK;
SELECT nextval('order_numbers'); -- 11
```

An ordinary `nextval` is not rolled back. A restart keeps private values until
commit, including when a prepared transaction survives loss of the local disk.
Sequence names share PostgreSQL's relation/type namespace: creating a sequence
with an existing enum's name fails with `42710` and leaves that type unchanged.

Function replacement publishes its body, signature and saved dependencies together
with its pending name. Routine comments follow the function's identifier, so
renaming one overload cannot rename another overload's comments. Dropping and
recreating a table, sequence, routine or materialized view starts a new lifetime
without inheriting the previous object's comment or permissions.
For example, `obj_description('order_total()'::regprocedure, 'pg_proc')`
reads a function comment. The one-argument form searches local object comments
across catalogs; an unknown explicit catalog returns NULL. Shared account and
database comments use `shobj_description`.

### Replacing views and refreshing saved results

`CREATE OR REPLACE VIEW` changes the query in place. The view, its row type,
array type and internal `_RETURN` rule keep their object identifiers. Owners,
permissions, comments and dependent views stay attached. Replacement works when
`max_views` and `max_rules` are already full; its pending definition and saved
dependencies still need their configured startup reservations.

```sql
CREATE VIEW report AS SELECT 1 AS total;
BEGIN;
CREATE OR REPLACE VIEW report AS SELECT 2 AS total;
SELECT total FROM report; -- 2
ROLLBACK;
SELECT total FROM report; -- 1
```

View and rule edits retain the selected object and revision through waits and
undo. A stale edit cannot modify a replacement in the same catalog slot. Dropping
and recreating a name starts with no comment; rollback restores the old object's
comment. This also applies to materialized views. Savepoints preserve this
distinction through repeated reuse.

A materialized view stores query results. `WITH NO DATA` leaves it unreadable:
ordinary reads return `55000`, including `LIMIT 0`. A constant-false `WHERE`
removes the scan and returns no rows. Direct `COPY` returns `0A000` instead.
`REFRESH MATERIALIZED VIEW` requires `MAINTAIN`, runs its saved query as the
view's owner, and retains its exclusive relation lock until transaction end.
The caller's account is restored after success or failure.

```sql
CREATE MATERIALIZED VIEW totals AS SELECT 7 AS total WITH NO DATA;
BEGIN;
REFRESH MATERIALIZED VIEW totals;
SELECT total FROM totals; -- 7
ROLLBACK;
SELECT total FROM totals; -- 55000: the refresh was rolled back
```

`pg_class.relispopulated`, `pg_matviews.ispopulated` and readable rows follow
transaction and savepoint rollback together. Journal and checkpoint recovery
preserve view, row-type, array-type and internal rule identifiers, including
gaps left by rolled-back creation. Tests delete the entire local data directory
before reopening from object storage; committed refresh results and prepared
commit or rollback decisions retain the corresponding readable state.
Tables, sequences, routines and materialized-view catalogs also retain their
exact saved positions and creation identities. A materialized view retains the
backing table's lifetime; a database template copy selects its copied table.
Unknown or occupied saved positions fail recovery rather than selecting a
replacement. Replaying a committed table drop releases its rows and dependencies
before a later creation reuses the saved position. For example, repeatedly
running `pg_restore --clean` against populated tables must leave the final rows
readable after restart. See the [current stored formats](durable-format.md).

When an index follows a table rename or schema move, its key and included columns
follow the table definition it selected. This includes an index created before
or between changes in the same transaction, and a key column added by that
transaction. Savepoint rollback restores the earlier binding. For example,
creating an index on `added`, then removing an earlier column, must keep the
index on `added` through commit and restart.
Index reads select the table lifetime directly instead of searching for a table
by its old name. Installation waits recheck both that lifetime and the selected
definition; a changed source returns `40001`. Unrelated tables remain available.
Each reserved pending-definition slot includes a 16-bit column transition map
and a revision number; each index slot includes its table binding. Startup
accounting uses the complete slot sizes, including padding. These controls do
not change durable formats or grow at runtime.
Index names and comments move with their table's schema inside the transaction.
A conflicting destination name rejects the move before publication. Other
transactions still see the committed name as occupied (`42P07`); a request for
the newly reserved name waits for the move or rename to finish. The moving
transaction can reuse its old name. Savepoints restore both names and comments.
Table deletion identifies its
indexes by the selected table lifetime, including after a rename or schema move.

An index built with a different text ordering cannot supply the column's normal
ordering. For example, an index on `name COLLATE "C"` does not remove the need
to sort `ORDER BY name` when that column uses another collation (text ordering).
The same check applies after a restart.

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
Replacing a rule preserves its enable mode. Rule renames reject an already used
name, including the rule's current name; the internal `_RETURN` rule cannot be
renamed.
Enable-mode commands for rules and triggers use `ALTER TABLE` on a table;
applying them to a view returns `42809` as in PostgreSQL. Ownership is checked
first (`42501` for a non-owner). A rule-mode command then waits for existing
readers and can hit its configured lock timeout (`55P03`) before the type check.
With MVCC enabled, a trigger-mode command takes a lock compatible with ordinary
readers and row locks from `SELECT ... FOR UPDATE`. Those readers do not delay
its `42809` rejection on a view or a valid trigger-mode change on a table.
Locking mode deliberately takes stronger locks; conflicting commands wait
until the reader finishes. Materialized views reject both rule and trigger
enable-mode commands with `42809`.

Tables, views and accounts have distinct lock identities. Account locks use
durable identifiers, so they do not transfer to a reused catalog position.
The lock registry reserves `(max_connections + max_prepared_transactions) ×
(max_tables + 1 + max_views + max_roles)` entries at startup; exhaustion returns
`54000` naming the registry. Prepared transactions retain view locks through
restart and recovery without local caches. Saved commit and rollback decisions
release the transaction's private rows and catalog entries before replay, so
recovery cannot wait on its own unfinished table or sequence creation. Live
resolution, startup recovery, and savepoint rollback share catalog undo.
A checkpoint keeps the committed definitions of objects waiting for a prepared
deletion. For example, preparing `DROP MATERIALIZED VIEW report` must not erase
its saved definition: `ROLLBACK PREPARED` still needs the original report.
The same committed-existence rule applies to catalog snapshot filters.
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
release their claims and leave the working list intact. A competing replacement,
changed table definition, or newer publication rejects installation with `40001`
before changing the published list. A cache whose row map fills is
marked incomplete, so it cannot supply an authoritative negative lookup.

Cache owners and their buffers are reserved at startup. Retained readers count
against `max_value_indexes`; a full claim pool returns `54000`. For example, a
replacement that runs out of slots can be retried after an older lookup releases
its cache. The pool must also fit the complete replacement beside the published
set: a table with two distinct key bindings needs two free buffers to rebuild
while its existing two remain usable. A failed claim returns `54000` and keeps
the working caches. Change this capacity before restarting; it never grows
automatically. Allocation controls release before clearing a claimed cache.
Committing an index change keeps its already prepared caches when all remaining
key definitions match; row publication has already installed the transaction's
row changes there. This finalization needs no second buffer set or row scan.
Dropped bindings release after their readers finish.
Creating an index also shares unchanged bindings and builds only new lookup data;
for example, adding an amount index keeps the existing order-number cache usable.
Table changes can keep these caches when rows were not rewritten, or when both
the table and its caches are empty. Changed key definitions still need new buffers.
Startup recovery recycles its unexposed cache buffers while keeping durable index
identities. It does not need space for two copies of every recovered cache.
Each table rebuild runs in its owning database; success and errors restore the
caller's database context. Runtime table-identity checks remain strict.

Index-key preparation uses one selected table shape for decoding the row and
evaluating its expressions and membership predicate. Ordinary column keys copy
only column types; they do not claim or copy a complete definition image.
A row from an older physical
layout is rejected with `40001` before decoding. Committing a row rechecks that
definition and the selected cache bindings before changing row history or caches.
An ordinary commit to another row does not invalidate prepared keys. For example,
rebuilding an order index between preparing and publishing a new order forces
that stale publication to fail with `40001`; it cannot insert an old binding's
hash into a new cache. These checks do not yet provide the roadmap's
whole-statement catalog capture or SQL execution on several CPU workers.

### Reading database descriptions while they change

With `transaction_mode = mvcc`, catalog listings retain the rows selected by
their statement or transaction. A read-committed statement can select newer
rows; a repeatable-read transaction keeps its earlier listing. These reports
build their results by joining the selected base rows:

- `pg_tables`, `pg_indexes`, `pg_roles`, `pg_group`, `pg_user`, `pg_shadow`,
  `pg_views`, `pg_matviews`, `pg_rules` and `pg_sequences`;
- `information_schema.tables`, `columns`, `views`, `sequences`,
  `view_table_usage`, `view_column_usage`, `table_constraints`, `key_column_usage`,
  `constraint_column_usage`, `constraint_table_usage`, `referential_constraints`,
  `domain_constraints`, `check_constraints`, `schemata`, `routines`, `parameters`,
  `enabled_roles`, `applicable_roles`, `administrable_role_authorizations`,
  `table_privileges`, `column_privileges`, `routine_privileges`, `usage_privileges`
  and their `role_*_grants` reports.

Remaining shared-publication work is tracked in [PLAN.md](../PLAN.md).

**User example:** an older repeatable-read report can still show a function's
previous name and SQL body after another connection replaces and renames it.
Its permission checks remain current: revoking EXECUTE can remove the function
from `information_schema.routines`, and a user who can execute it without owning
it sees a NULL definition. Parameter names and types come from the selected
description; parameter defaults are looked up in the current function. The
routine and parameter reports expose PostgreSQL's complete 82-column and
32-column result shapes, including the built-in routines represented in
`pg_proc`.

Schema listings show the current database name and only schemas the user owns
or can access. Account reports retain selected login settings and group members;
`pg_user` masks passwords and `pg_shadow` requires a superuser. Grant reports
retain selected grants and account names while checking current role membership.
For example, if a role inherits the owner's rights, the report can say that it
may pass a grant on. Overlapping table and column grants follow PostgreSQL's
rules for combining rows before that check. These reports follow the
[PostgreSQL 18.6 definitions](https://github.com/postgres/postgres/blob/REL_18_6/src/backend/catalog/information_schema.sql).

Column descriptions retain PostgreSQL's physical type properties. For example,
`pg_attribute` reports alignment `s` for `smallint`, `d` for `bigint`, and `d`
for `bigint[]`. These describe PostgreSQL's catalog contract; pos3ql chooses
its object block layout separately.

Catalog expression values have their own validated type. A user can copy
`pg_constraint.conbin` into a `pg_node_tree` column and later display it with
`pg_get_expr`. Supplying text or binary input for that type fails with `0A000`,
including text that resembles the internal encoding. PostgreSQL makes this
same distinction between copying a node and accepting client input.

Removing an earlier column does not change the numbers of surviving columns.
For example, after creating `parcels(old_code text, weight bigint CHECK(weight>0))`
and dropping `old_code`, a metadata report still identifies `weight` as column 2.
The dropped column remains an entry in `pg_attribute`; a new column receives
number 3. Saved checks, indexes, permissions and comments keep referring to
`weight`. Rollback, prepared transactions and recovery preserve those numbers.
PostgreSQL's 1,600-column table limit includes these retained dropped entries;
an addition beyond that limit fails with `54011`.

Use `UNIQUE NULLS NOT DISTINCT(package_id, attempt)` when two rows such as
`(17, NULL)` must conflict. The second insert fails with `23505`. Choose
`UNIQUE NULLS DISTINCT` to permit those rows. These explicit choices work in
table and column constraints, `ALTER TABLE`, conflict handling, deferred checks,
table copying and recovery. Constraint reports expose the selected choice.
Renaming a table or column preserves its stored NOT NULL constraint name.

Object listings use the PostgreSQL metadata inventory independently of which
catalog row queries are implemented. For example, `pg_statistic` appears in
`pg_class` and resolves through `to_regclass`, while reading its raw rows fails
explicitly with `0A000` (unsupported feature).

**User example:** start a repeatable-read report listing `orders`. Another
connection renames it to `sales` and commits. The report can still list
`orders`, while `to_regclass('sales')` resolves the current name. An owner's
displayed account name can also change after that account is renamed. Listing
an old definition and resolving an object for use are different operations.

`pg_get_viewdef` and `pg_get_ruledef` read the selected rule text. For example,
replace a view's `SELECT 1 AS value` with `SELECT 2 AS value` in another
connection. An older repeatable-read report can still show the definition
containing 1, while executing `SELECT * FROM the_view` returns 2. This follows
[PostgreSQL's rule lookups](https://github.com/postgres/postgres/blob/REL_18_6/src/backend/utils/adt/ruleutils.c).
Retained rule text uses the existing catalog-history reservation; its internal
fields are unavailable as SQL columns.
If another connection drops the view, that older report can instead fail with
`XX000`: the selected rule still exists in its snapshot, but the relation needed
to print it no longer exists. A fresh read-committed listing omits the view,
and looking up its old identifier with `pg_get_viewdef` returns NULL.

The same report can keep a view's old name and check option, a materialized
view's selected index and population flags, and a sequence's earlier increment.
The sequence's latest value and its access checks remain current. For example,
after another connection changes an increment from 2 to 5, an older
`pg_sequences` listing can still show 2 while `last_value` reflects a later call
to `nextval`.

Information-schema reports apply current access checks. A user with SELECT on
someone else's view can list it, but its definition is NULL unless its owner
role is enabled. View dependencies list referenced tables and columns only
when their owner role is enabled, and repeated references produce one output
row. Identity sequences are excluded from `information_schema.sequences`.
Permission checks keep numeric relation identifiers through lookup, including
objects outside the search path that share a name with another schema's object.
These joins and permission rules follow the pinned
[PostgreSQL definitions](https://github.com/postgres/postgres/blob/REL_18_6/src/backend/catalog/information_schema.sql).

`pg_sequences.data_type` has type `regtype`; information-schema names have type
`name`, and its textual details have type `character varying`. System view
definitions come from retained PostgreSQL 18.6 metadata, with provenance in the
generated source. The PostgreSQL comparison checks those definitions on each run.

Set `catalog_history_bytes` explicitly before startup; `16MiB` is one example
for MVCC mode. It reserves space for earlier catalog rows. A full reservation
returns `54000` before making the change durable. Rollback and savepoints release
unfinished reservations; old readers retain the committed rows they need.
Locking mode requires `catalog_history_bytes = 0` and waits for conflicting
operations instead of retaining these earlier catalog rows.

Catalog object identifiers have type `oid`, and `tableoid` is a hidden system
column: name it explicitly rather than expecting it in `SELECT *`. Index
vectors such as `pg_index.indoption` start at zero. For example,
`indoption[0]` describes the first key; included columns have no option entry.

Renaming a source column preserves a view's existing output names. For example,
after `CREATE VIEW order_report AS SELECT customer_id FROM orders` and
`ALTER TABLE orders RENAME customer_id TO account_id`, use
`SELECT customer_id FROM order_report` as before. The view keeps reading the
same source column. Rollback restores its earlier definition, and recovery
retains the committed binding.

Catalog vectors cast to ordinary arrays while keeping zero-based subscripts.
For example, `('9 10'::oidvector)::oid[]::text` returns `[0:1]={9,10}`.
The reverse SQL cast, `oid[]::oidvector` (or `smallint[]::int2vector`), fails
with `42846`, matching PostgreSQL. `oidvector` equality compares its values;
its ordering compares length before values. See the
[PostgreSQL 18.6 comparison implementation](https://github.com/postgres/postgres/blob/REL_18_6/src/backend/access/nbtree/nbtcompare.c).
Binary input must be nonempty, start at zero, and contain no NULL elements.
Empty vectors still have PostgreSQL's one-dimensional binary
result header; sending that result back as binary input is rejected by
[PostgreSQL's receiver](https://github.com/postgres/postgres/blob/REL_18_6/src/backend/utils/adt/oid.c).

### What permission lookups see

`pg_catalog.has_language_privilege('sql', 'USAGE')` has the Boolean result
type, as do the other supported privilege, visibility and publication checks.
The type stays Boolean when an argument is NULL, so clients receive type
identifier `16` in both simple-query and prepared-statement descriptions.

Catalog permission rows retain role identifiers. Renaming a role changes the
name printed in an older permission list without adding later grants to that
list. `pg_get_acl`, like PostgreSQL's object lookup, reads current permissions;
an explicit catalog query still follows its statement snapshot. For example,
an older repeatable-read query can list a role's SELECT grant while
`pg_get_acl` also reports a later UPDATE grant for the same object.
Its accepted object classes, large-object alias and sub-object numbers follow
[PostgreSQL 18.6's object lookup](https://github.com/postgres/postgres/blob/REL_18_6/src/backend/catalog/objectaddress.c).
For example, a class without an ACL returns NULL, while an unrecognized class
raises `XX000`; these cases are distinct.

Permission lists keep grant order when storage slots are reused. Removing a
grant and adding it again puts it at the end. Removing the last grant leaves
an explicit empty list (`{}`), which survives restart; a later SELECT grant
adds only that grant. An owner can revoke its own SELECT permission while
retaining the right to grant SELECT. Concurrent changes to one object's list
wait for its transaction, while changes to other objects can proceed.

### How statistics reads and journal batches stay consistent

`ANALYZE` collects estimates used to choose query plans. Relation counts and
column estimates have different PostgreSQL lifetimes: rolling back can leave
`pg_class.reltuples` changed while `pg_stats` column estimates revert. Reads
copy the image visible to their transaction under short guards. The private
version pool, rollback, commit, replay, and table reuse use the same ownership
order. Readers retain their copied values without holding these guards.

Creating an index also refreshes the table's row estimate using the validation
scan. `VACUUM` counts resident and saved rows in batches and preserves column
estimates unless `ANALYZE` was requested. A table's `relhasindex` flag can remain
true after an index is dropped or its creation is rolled back. `pg_index` lists
the actual visible indexes; `VACUUM` clears the flag when none remain. The flag
belongs to the table's lifetime and cannot leak to a replacement table.
An earlier table rename gives the transaction a private catalog row. Creating
an index then changes only that private row; rolling back restores the old flag.
This remains true if the transaction renames the table back before creating
the index. Without that prior edit, PostgreSQL updates the existing flag in
place, so other connections can see it before commit and after rollback.

**User example:** after `DROP INDEX orders_lookup`, a catalog query can still
show `relhasindex = true` for `orders`. Running `VACUUM orders` clears it when
the table has no remaining indexes, and refreshes its row estimate. Restarting
with empty caches preserves the saved flag and estimate. An unresolved prepared
`ANALYZE` restores its relation counts while its column estimates remain private.

Journal capture copies the visible estimates and records the exact relation-count
revision and index flag from the same cell. Acknowledgement clears the pending
marker only when both still match; a newer count or flag remains pending. Reused table slots preserve
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

Saved-file references use table-local identities rather than positions in a
file list. An adjacent merge publishes one file covering both input identities;
full replacement covers the earlier identities. Publication holds short table
and file-list guards without walking rows or locking the row-version pool.
Existing reports retain their original files and identity mapping.

For example, a report reading yesterday's order values can continue while
maintenance merges order files. New queries read the merged file. Neither the
report's selected values nor the row references need to change.

Identities are reserved before publishing a durable checkpoint. A failed attempt
consumes its reservation; clearing a list never reuses identities. The 32-bit
table-local identity limit returns `54000` with
`table SST generation identity space exhausted` before publication. Pending
installs belong to their storage instance, table lifetime and rewrite revision;
they cannot install into a replacement table. Their controls and reader mappings
are included in startup memory reservations. Durable file bytes are unchanged.

Paced merges keep the historical baseline, and byte scans skip versions newer than
the selected snapshot. Maintenance that replaces a complete saved history remains
guarded until that operation retains a complete historical source. Merge scratch
exhaustion returns `54000`.

A startup-sized registry protects durable and temporary object graphs used by
readers. Garbage collection copies retained root references before object I/O.
Releasing a root used by a durable garbage pass schedules more maintenance even
without another write. Active backups also retain their reachable objects.

A paused report retains its selected saved row and index files after its scans
and statement arena are released. For example, a report looking up an invoice
through an index keeps that index's files until the report finishes, even if a
later checkpoint replaces the saved index. Nested order and invoice reads can
finish their individual walks while the outer report still needs those files.
Resuming adds any newly selected files to the same statement owner. Completion,
SQL errors, cancellation, rollback and timeout release it; an executing scope
keeps its files even if its snapshot or Storage instance is retired first.
Streamed COPY restores that owner for each data message and keeps files selected
by its triggers until completion or abort, including while it waits for input.
Checkpoint uploads retain separate request owners. Source reads within atomic
maintenance can still wait synchronously without discarding a paused report.

File owners use the same 1,024 slots per query workspace as statement snapshots,
independent of connection count and the separate 64-slot point-source pool.
Each shared row or index file slot reserves one membership bit per statement slot
at startup; repeated aliases share that bit. Index files have an independent
reservation of `2 * max_value_indexes` distinct roster roots; metadata changes
that name the same immutable graph share a slot. Its full selected handle keeps
its own entry count and publication position. Capture pins the graph under table
publication ownership, then releases that ownership before I/O. Persistent index
callbacks also release the block-store lock before nested reads. SQLSTATE `54000` reports
"statement file owners are exhausted (N)", "statement file identity exhausted",
"statement file references exhausted" or "statement file scope nesting exhausted".
File-owner exhaustion fails before selecting or registering a snapshot; it does
not grow memory or partly pin another file.
A snapshot used with another storage instance returns `XX000`, "statement
snapshot belongs to another storage instance", before selecting its file context.
Index capacity errors are "retained index files exceed the startup capacity (N)",
"retained index file reader count exhausted" and "retained index file snapshot
exceeds garbage collection scratch (N)". Row-file capacity remains independent.
Garbage collection walks retained, current and backup index graphs through the
same storage interface. Its startup-reserved membership map counts each distinct
block once against `checkpoint_live_blocks`, including shared backup blocks;
real exhaustion returns `54000` before deletion. Invalid rosters return `58030`.
The membership map is charged separately from the existing checkpoint workspace.
An unrelated commit does not invalidate a table's index view when a read resumes.
For an older snapshot, eligibility checks each binding's last change and
saved-generation position. Newer key, predicate or included-value changes
still require the ordinary historical row scan. For example, an order update
does not make a paused invoice lookup abandon its required index download;
a missing response still returns `58030`.

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

Heap relocation keeps queries running while it moves cached rows. For example,
a report can keep its selected order total of 10 while another connection writes
a total of 20. Both retain their own bytes, even when the old and new heaps use
the same byte offset. Heap addresses describe caches, not durable row identity.

The startup workspace reserves the spare heap and captures locations in turns
of at most 64 stable row slots, including empty or deleted slots. An empty table
costs one visit. Sorting, validation, destination assignment and copying share
65,536 work units per ordinary turn. A sorting unit starts one heap-sort root,
extracts one entry, or sifts one level (at most two comparisons and one swap).
Validation and packing charge one unit per captured location; copying charges
one per byte or skipped alias/zero-length entry, at most 64 KiB per turn.
Overlapping nonempty locations return XX000, "heap compaction contains overlapping
row locations", before publication, including across zero-length entries.

Row-reference preflight and address updates also visit at most 64 stable slots
per ordinary turn. The quota counts slots: one row can have retained versions.
Each batch owns one table and its version chains, then releases both owners.
A busy owner retains the cursor for the existing one-second retry without
changing storage readiness. No all-table row-map lock is acquired by this path.

Before switching heaps, new references to an already captured prefix are copied
into the append tail. This includes database-template clones into reused slots.
Later appends are copied before the switch; a growing tail requests another
copy turn. The switch briefly owns version publication and the heap controls.
Both heaps remain readable while subsequent turns update committed rows,
history and pending writes. New publications use the current heap, so rollback,
late writes and slot reuse cannot restore an old address behind the cursor.

The published heap owns the mapping and progress until all references have been
updated. Dropping the original maintenance task cannot discard them; another
startup workspace can resume the published move. Errors after the switch retain
its mapping and completed updates. Cancellation before the switch releases the
private copy. Using the original workspace against another storage instance
returns XX000, "heap relocation belongs to another storage instance", and leaves
its original task intact. Moving the engine does not invalidate either task.

Both heap buffers, shared controls, mapping storage and progress are charged
exactly at startup. Retained ordered row records remain 32 bytes; their source
explicitly identifies the old or new heap. Exhaustion returns 54000 and names
"heap compaction scratch overflow", "row heap relocation buffers are retained by
readers", "row heap relocation is already active", or "heap relocation scratch
is retained by publication". Explicit synchronous callers also reserve one
optional row-map guard per physical table slot (`max_tables + 1`), including
alignment and mutex/arena controls; an undersized reservation returns 54000,
"heap row-map ownership workspace exhausted", before publication.

Automatic maintenance runs bounded turns between messages and while idle,
including while an unrelated object download is waiting. At 85% heap use,
top-level writing commands wait before executing their expressions or triggers
when checkpoint publication or local cleanup is already in progress. Prepared
writes use the same admission rule; prepared reads remain runnable. Temporary
row-map pressure also waits for an active spill cleanup. For example, an INSERT
can wait for space while another connection reads its orders, then resume
without repeating an earlier SELECT or its own sequence increment or trigger.
The reactor wakes this wait when capacity changes, cleanup ends, or maintenance
reports an error; an unrelated object GET is not retried on those wakes.
`pg_stat_activity` reports Extension/CacheCapacity for the wait. Cancellation
releases the retained frontend command through the normal protocol boundary.
The statement keeps its original timestamp and timeout while waiting. SQL inside
functions and triggers shares that deadline. It wakes the server even when no
storage request or cleanup turn completes.
New maintenance errors reach already waiting commands; an earlier failure does
not poison a later request. For example, correcting object-store credentials
allows subsequent inserts to wait for space and continue without inheriting the
old authentication error.

A transaction's own pending writes or DDL do not wait for space they might pin.
Transaction completion, rollback, explicit CHECKPOINT and VACUUM remain runnable.
Once the active task ends, an operation that cannot fit still receives its named
capacity error. This admission rule does not make a single large statement or a
routine invoked from SELECT resumable midway through its effects. Explicit
CHECKPOINT and shutdown finish cleanup synchronously and may reclaim an
unretained active heap in place.

Cleanup releases history and evicts eligible saved rows before moving the
remaining heap once. Permanent cleanup selects saved heap rows at 50% allocated
heap use; below that it selects pressured row maps. Temporary cleanup also
compacts when history pruning alone released heap references.

Post-checkpoint history release, heap-row eviction and redundant-entry removal
also use at most 64 stable row slots per turn. Busy tables are skipped and
retried; partial acquisition releases all earlier owners. A pressured table
remains selected across partial batches after its occupancy falls. Durable
history pruning preserves newer versions; local history pruning checks current
readers under the same registry owner used for admission. Temporary rows use
their own committed revision through the saved boundary, so a later write does
not prevent eviction of unchanged saved rows.

Pending changes and retained history stay resident. Ordinary metadata eviction
also keeps heap rows, deletion markers and unpublished saved rows. Checkpoint
installation clears only deletion and change markers covered by its captured
boundary, in at most 64 slots per turn; later writes remain dirty. Journal
cleanup preserves the suffix after that boundary for recovery. The same startup
state machine handles temporary history, saved rows, compaction and metadata.

These ownership and scheduling rules do not enable SQL execution on several CPU
workers. Engine-wide catalog coordination, remaining table rewrites, metadata
capture and some atomic maintenance reads still need further work. Checkpoint
metadata uploads and immutable table, index and merge output retain their own
bytes while network requests overlap.

For example, while one connection downloads an uncached invoice, another can
update a cached order and receive its durable commit acknowledgement. INSERT,
UPDATE, DELETE, COPY, trigger execution and commit use the existing separate
block client for any reads that must finish without restarting the operation.
They preserve the suspended reader's socket, response bytes and error, including
across nested calls and SQL errors. The reader keeps its original statement
snapshot and command identity across suspension, while a later READ COMMITTED
statement selects a fresh view. Its statement timestamp and absolute timeout
are restored too; transaction timestamps remain owned by their transaction
when connections take turns. The wait state is inline in startup-owned
transaction controls and retains an existing fixed snapshot slot. Cancellation,
rollback, completion and errors release it. A write that needs an uncached block
still waits synchronously; it can
issue another GET even if a reader is already downloading that block. SQL
execution remains serial. Checkpoint uploads can progress independently;
SQL worker threads remain separate roadmap work.

Row lookups retain the verified sparse-index page already in their own buffer,
including the row and version bounds where it is valid. A resumed report can
reuse navigation across rows and snapshots without downloading the same page
again; another source or an out-of-range key requires a new verified page.
Scans retain verified blocks and completed columns by immutable source and block
position in their existing frames. A nested order scan can reuse another frame
without discarding an invoice scan's work. Cursors check a frame's source before
reusing it after another walk. Startup sizing charges the per-frame identities
and column metadata; byte-buffer counts do not grow.
Index readers keep verified metadata and entries in their existing startup
buffers when a later row read waits for a download. Selected-column scans keep
completed columns in the same way. Their identities follow the immutable source,
so another attempt can reuse the bytes even after shared-cache eviction.
The fixed pools prefer buffers already used for the same source, then unused
buffers, so reading a second table does not overwrite the first table's work
while another buffer sits idle. Point lookups also retain completed membership checks and the selected data
reference while that data downloads. A metadata probe leaves decoded rows
available for the following fetch. Overwriting scratch or failing a replacement
clears its previous identity.

Deferred constraint triggers run before an implicit commit at the end of a
simple query, extended-protocol Sync or COPY. For example, if a deferred order
check rejects a copied row, COPY fails and its transaction rolls back; the
connection can issue another command. These commits use the same trigger-aware
boundary as explicit COMMIT and preserve pending downloads throughout.

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
when the snapshot already covers them. Journal publication uses its own upload
clients and can finish while a reader waits for an unrelated block. Captured
physical output and metadata retain separate bytes and completion state.

Eviction requires an installed object's recorded boundary to cover the row's
commit position. A later update stays in the heap; history newer than that
boundary stays resident too. Recovered roots have no eviction boundary and do
not justify discarding newer heap bytes. A temporary spill records the table
generation it copied, because temporary commits may share a zero journal
position. Each newly committed image records its table revision in startup-charged
row metadata. Cleanup can evict an unchanged row covered by that capture even
if another row changes meanwhile; a newer image stays in memory until saved.
A later spill names its own object. A committed temporary deletion is distinct
from a row that never committed, including at journal position zero; saving or
rolling back another change cannot make the deleted row reappear. These boundary
kinds are distinct states in the startup-sized root metadata. For example, if a checkpoint saved an order before its price
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

Enum row values retain a stable member identity rather than depending on a
label's spelling or sort position. This also applies inside named records and
arrays, column and domain defaults, and typed constants captured in views.
A typed enum argument inside a default also retains its member when a function
runs for every new row. For example,
after `ALTER TYPE order_state RENAME VALUE 'pending' TO 'waiting'`, an older
transaction reads `waiting` from its unchanged order row. A view created with
`'pending'::order_state` follows that same member; adding a new member named
`pending` does not redirect the view. Plain text containing `pending` is unchanged.
The label edit changes catalog metadata and does not rewrite the orders table.
Rollback selects the earlier definition again.

Domain rules also apply when an enum value is stored inside a record or an array.
Changing a label or moving its type keeps those rules and their captured values
attached to the same enum members. PostgreSQL requires a base-type cast when
comparing a domain over an enum. For example, with a domain `order_state` over
`state`, write `status::state = 'ready'::state`; `status = 'ready'` returns
SQLSTATE `42883`. The explicit cast does not remove checks when assigning a
value to the domain.
PostgreSQL keeps the original text in `pg_type.typdefault`. For example, after
renaming `old` to `new`, a domain default inserts `new` while that cached catalog
field still shows `'old'::state`.
[PostgreSQL's information-schema view](https://github.com/postgres/postgres/blob/REL_18_STABLE/src/backend/catalog/information_schema.sql#L943)
exposes that same saved domain text. Column and view default descriptions and
execution use the retained member, so they do not interpret that old text again.
For a domain, query `pg_get_expr(typdefaultbin, 0)` for the current default
expression. This separate field follows renamed labels and type names without
running functions in the default.

Selected enum definitions and label iterators retain the label image they read.
A label image is one complete saved list of labels.
Changing or dropping the type, rolling back a later change, reusing its catalog
slot, replaying a definition, or retiring a database cannot replace those
selected labels. Iterators release their image at the end or on drop; cloned
definitions release it when dropped. The label buffers remain alive after Storage is
removed while a selected reader still owns it. For example, a reader that
selected the status labels `new` and `paid` keeps those labels while an ALTER
adds `shipped`; another lookup sees the new definition.

Enum label edits and name moves use one shared storage commit API.
An active storage reader can retain both its table definition and selected labels
while a label-only update completes. A name or schema move additionally acquires
the dependent definitions before changing the type and its references together.
Undo records identify the transaction, the specific creation of the type, and
its pending revision number. An older undo cannot release a newer edit's label
buffer, including after a slot is reused. Revisions use a separate checked
type-change counter, preserving durable object identities. All storage is reserved at startup.
Server statements still execute serially.

Committed definitions, pending changes and retained readers share the existing
startup label buffers. A separate image remains reserved for replay.
Image counters and controls are charged at startup. Exhaustion returns 54000,
"enum member-image pool is exhausted", without changing the published
definition. Release old readers or increase the startup catalog reservations
before retrying. Replay can reuse an unretained image in place; it reserves a
different image when readers retain the old one. Named-type creation uses shared
storage access; some engine metadata preparation still requires exclusive access.

Transaction-wide catalog coordination, remaining table rewrites and SQL execution
workers remain open.
Concurrent and batched object-store I/O and ordered durable publication are
implemented; the storage and transaction guides describe their limits.

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
