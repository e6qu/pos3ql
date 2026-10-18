# What remains to finish pos3ql

[Overview](README.md) · [What works today](docs/implemented-baseline.md) ·
[Compatibility limits](docs/postgresql-18-compatibility.md) · [Contributing](CONTRIBUTING.md)

## The destination

Use familiar PostgreSQL SQL and tools while keeping durable data in object
storage. Reads, writes, and background storage work must progress together,
within a memory budget chosen before startup. Recovery, replacement, and
performance claims must have repeatable evidence.

For a user, this means a report can read the right version of an order while
another transaction updates it, an unrelated update need not wait for a slow
object download, and a confirmed commit survives loss of the server's disk.
One active server writes to each object-store prefix; many transactions inside
that server should execute at the same time.

The [compatibility guide](docs/postgresql-18-compatibility.md) defines supported
behavior and features outside the design. Unsupported operations must return
clear errors. Memory reservations must not grow after startup to rescue a query.
Use explicit settings and arguments for consequential choices; avoid needless
wrappers and fallbacks. Earlier pos3ql versions are not compatibility targets.
PostgreSQL SQL and wire behavior remain compatibility requirements.
Recovery requires the current checkpoint metadata format, including the writer
identity and counters for newly assigned values. Startup checks the format and
required fields of the checkpoint and latest commit record before changing
writer ownership. Recovery is tested after deleting local caches.

## Where we are

| Area | What works today | What still needs to be built or demonstrated |
|---|---|---|
| SQL and clients | Broad PostgreSQL 18 SQL, types, catalogs, transactions, procedures, drivers, and dump/restore | Keep results and explicit limits correct through the changes below |
| Saving and recovering data | Object-store commits, checkpoints, indexes, caches, and recovery without local caches | Keep those guarantees when queries and background work execute together |
| Running the server | Backups, export to another prefix, recovery to a saved time, probes, credential rotation, packages, and replacement tools | Demonstrate recovery and failover across machines with actual client routing |
| Concurrent work | Separate statement buffers and protection for many reader and writer data structures | Finish protecting all shared state and enable a fixed number of execution workers |
| Measurements | Fixture, MinIO, and SeaweedFS tests, plus vanilla PostgreSQL controls with available and matched CPU/memory | Publish repeated measurements on fixed hardware with separate object storage |

**Statement execution is still serial.** Reserving more workspaces does not
make queries execute in parallel. Existing recovery tools and tests on one
machine do not establish production availability or speed.

## What the remaining work must preserve

- A paused operation keeps its selected data and its progress. Resuming must
  preserve earlier validation and run each data-changing function only once.
- A report may read older orders while a uniqueness or constraint check needs
  current rows. Keep the correct view for each operation.
- Readers and backups may still need older object versions. Each rollback and
  publication must leave valid state before independent storage work resumes.
- MinIO and SeaweedFS tests check portability. Performance claims need repeated
  measurements on fixed hardware with independently operated object storage.

The [history index](docs/history/README.md) and linked PRs preserve completed
investigations; current behavior belongs in its owning technical guide.

## Remaining sequence

### 1. Keep data valid for readers while it changes

**User example:** a report started before an order update must see the version
appropriate to its transaction. Dropping or rebuilding a table must not leave
an already running read following reused memory or deleted objects.

Readers retain selected rows, definitions, trigger inputs and index caches
through changes and replacement, within startup reservations. Per-table
ownership covers creation, private edits, name lookup and type references.
Enum values retain member identities through label changes, including inside
records, arrays, defaults and views. Label edits use shared storage access;
old undo records cannot erase later edits or replacement types.

Row writes, replay and table rewrites retain the bytes they need. Automatic
local cleanup sorts, validates, assigns destinations and copies rows in bounded
turns, including while an unrelated object download waits. Ordinary queries can
progress between turns. Row reads, writes and cleanup release shared owners
before waiting for one table. Post-checkpoint eviction and history release use
shared storage access. Reader-history cleanup visits at most 64 stable row slots
per ordinary maintenance turn, including while idle or an unrelated download
waits. Starting a statement and normal transaction completion schedule work;
they do not drain a table or copy all row identities. Busy owners retry without
changing storage readiness. A startup cursor preserves partial work, skipped
tables and new releases. Row publication maintains a pressure signal so testing
history pressure also needs no whole-table scan. Synchronous history completion
is an explicit internal choice. Heap capture, final remapping, post-checkpoint
eviction and other cleanup walks still have whole turns; explicit CHECKPOINT
and critical heap pressure still wait for cleanup.

Complete type-name changes and remaining maintenance still need publication
coordination. The [implementation guide](docs/implemented-baseline.md) owns
completed contracts, user examples and exact capacity errors.

Still to do:

- Finish shared-state protection for engine-wide publication and remaining
  table rewrites and maintenance paths.
- Move the remaining maintenance work off the statement path, including heap
  capture, remapping, critical memory pressure, and fair retries when
  both buffers are retained.
  Complete rewrites must preserve historical object versions needed by readers.
- Verify release after errors, rollback, and transaction end, and prevent
  reused identities or names in another database from being mistaken for older data.

Done when: an unrelated write can complete while a reader retains old data;
maintenance preserves that data; failed publication leaves the prior state
valid; all reader reservations are counted at startup; exhaustion, identity
reuse, rollback, and recovery are tested without runtime allocation.

### 2. Let storage requests and maintenance overlap

**User example:** one customer reads an uncached invoice while another reads a
cached invoice. The second request should not wait for the first download.
A checkpoint or storage merge should allow ordinary queries to keep progressing.

Local heap cleanup progresses during pending downloads without using an object
client. Failed maintenance has a one-second retry deadline even under continuous
client traffic or when idle; local progress does not clear a storage failure
status. Completing a published checkpoint's remote cleanup can
restore readiness without requiring another write.
An unrelated cached write, including COPY and commit, can complete while a cold
read waits. Atomic writes use the existing separate block client without
replacing a suspended reader's socket, bytes or error. A write that needs an
uncached block still waits synchronously and can download the same block again.
A paused report keeps the same data view, start time and timeout until it
finishes; resuming does not start a fresh statement.
Its selected saved row and index files stay protected after a scan, nested call
or statement arena is released; their owners follow the statement through waits.
An index's saved files are protected before its published reference can change.
Index callbacks release the block-store lock before calling nested reads.
Index eligibility uses that table's binding changes and saved generations;
an unrelated commit does not switch a paused lookup to a different access path.
Index and selected-column read buffers retain completed data across waits;
their verified identities do not depend on shared-cache residency.
Checkpoint publication and object merges still wait for downloads to finish.
Startup sizing and construction charge every object client, request-slot record,
block scratch buffer and checkpoint workspace before serving queries.
Commit-history cleanup follows the published chain one descriptor per turn;
unpublished uploads cannot replace the boundary needed for recovery. Unacknowledged
journal batches remain available until their publication retry completes.

- Keep cache checks and updates brief; release broad locks during slow object
  I/O and reserve any additional independent request workspaces at startup.
- Coordinate requests for the same missing block, allow bounded parallel reads
  and prefetch, and release requests correctly on cancellation.
- Build checkpoint and merge output separately, then publish the new object
  references in a short, consistent update. Keep the existing pending-download
  gate until publication and deletion use the retained row and index owners.
- Share limited resources fairly among reads, durable commits, and maintenance.
  Delete old objects only after readers and backups release them.

Done when: warm reads and unrelated writes progress during cold reads and
maintenance; requests stay within startup limits; recovery and deletion remain
correct; downloaded bytes, request counts, and rewritten bytes are measured
through the same storage interface for every supported provider.

### 3. Coordinate transactions and durable commits

**User example:** two customers update different orders. Each receives success
only after its change is recoverable from object storage. If the connection
breaks during publication, the error must not falsely promise that the change
was either saved or discarded.

SQL, maintenance, and recovery share checked row, transaction, catalog, and log
position counters. Log reservations reach the common journal-staging error path,
so exhaustion preserves existing failure cleanup; later observations cannot move
the global position backward. Catalog identities are reserved before publishing
compound definitions. Creating a transaction reserves its identity and active-registry entry
together; full capacities fail explicitly and leave existing owners intact.
Simple queries, protocol synchronization and COPY run queued deferred triggers
before an implicit commit; trigger failures roll back before durable success.

- Protect journal staging, prepared transactions, commit order, and the
  responses waiting for durable storage.
- Let execution progress without holding the whole engine locked while a
  commit waits on object storage.
- Preserve transaction visibility, lock ordering, shared commit uploads,
  cancellation, retries, savepoints, and recovery of prepared transactions.

Done when: concurrent fault tests demonstrate correct visibility, durability,
and explicit unknown-outcome errors within the fixed startup memory budget.

### 4. Run statements on a fixed set of workers

**User example:** a busy application uses several connections. Adding workers
should increase useful work completed per second while keeping results correct
and memory bounded. Too much incoming work must wait or fail explicitly.

- Replace the current serial queue execution with workers reserved at startup.
- Preserve connection/database identity, queue order, fairness, cancellation,
  storage waits, and commit ordering.
- Measure read-only, write-heavy, and mixed workloads from one worker through
  the supported maximum, including overload.

Done when: workers provide useful scaling and reads, writes, cold misses, and
maintenance overlap. A global lock that makes every statement wait its turn
does not meet this requirement.

### 5. Prove performance and recovery in a realistic deployment

**User example:** someone choosing between pos3ql and PostgreSQL needs to know
how long typical and slow requests take, what hardware they need, how much
object storage is read or rewritten, and what happens when a server fails.

- Run the [measurement suite](docs/performance.md) repeatedly on fixed hardware
  with an independently operated S3-compatible service. Record hosts, storage,
  network, routing, and owner-only configuration paths. No representative run
  has been published yet.
- Keep both vanilla PostgreSQL 18 controls: one with available host resources
  and one with the same CPU/memory allowance as pos3ql. PostgreSQL uses ordinary
  local durable storage; report that medium and its settings alongside pos3ql's
  object store, network, and caches.
- Measure warm memory, warm disk, empty caches, writes, maintenance interference,
  joins, large catalogs, workers, logical replicas, replica delay, recovery, and
  failover across machines with real client routing.
- Retain raw results, latency distributions, throughput, CPU, memory, access
  paths, object requests, downloaded bytes, and rewritten bytes. Evaluate row
  packing, selected-column reads, pruning, and compaction by those costs.

Done when: published repeatable results support the stated speed, recovery,
replica-delay, memory, and storage-cost claims. Same-host MinIO and SeaweedFS
results establish portability and exploratory costs; timing thresholds need
repeatable measurements before becoming gates.

## Rules that apply throughout

Keep accepted SQL widths, results, errors, catalogs, journal replay, checkpoints,
and empty-cache recovery correct in every step. The
[capacity inventory](docs/postgresql-18-compatibility.md#capacity-boundaries)
distinguishes PostgreSQL limits from smaller implementation limits and
configurable startup reservations. Durable-format changes must update the
[format inventory and recovery checks](docs/durable-format.md); unsupported
identities fail explicitly. Retaining old pos3ql readers or providing an upgrade
migration is not a completion requirement.

Keep test dependencies reproducible and verify their installation before running
comparisons. Fix interrupted downloads with bounded retries and verified caches;
do not extend test timeouts or accept partial setup.

The roadmap is finished when the steps above pass their stated checks and the
supported SQL, formats, monitoring, credentials, packages, backup/restore, and
replacement remain verified together. Keep current behavior in its
[owning guide](docs/README.md), unfinished work here, and dated observations
with their original [evidence](benchmarks/README.md).
