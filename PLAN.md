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

## What recent work taught us

- Protecting a reader's selected data and releasing it at the right time is
  necessary before allowing concurrent writes. A running statement, a
  transaction, and a streaming upload can keep data alive for different periods.
- Comparing exports with PostgreSQL exposed a wrong assumption about child
  tables. A table-form export reads only its named relation; the query form
  selects a complete partition or inheritance tree.
- A report and a uniqueness check can need different views of the same rows.
  Keeping a report stable must not let an index miss a later committed duplicate. A new
  table CHECK can inspect current rows while a STABLE SQL function inside that
  check still sees the transaction's earlier data.
- A safe storage layout needs to keep older versions until every reader and
  backup has finished with them. Copying only today's logical rows is not
  sufficient when an older transaction still needs historical object data.
- Testing multiple object-store implementations checks portability. Comparing
  them on a shared machine does not establish their production speed or cost.

These conclusions shape the sequence below. The [history index](docs/history/README.md)
and linked PRs preserve the investigations; this file keeps only current work.

## Remaining sequence

### 1. Keep data valid for readers while it changes

**User example:** a report started before an order update must see the version
appropriate to its transaction. Dropping or rebuilding a table must not leave
an already running read following reused memory or deleted objects.

Statements, transactions, streaming uploads, and imported snapshots retain
which committed data they may read. Captured reads and selected UPDATE, DELETE,
and MERGE targets keep their original bytes; sorting, nested SQL, and table
replacement cannot redirect those reads. Trigger-side changes preserve
PostgreSQL's command boundaries. Internal metadata walks check the captured
table identity during point admission. Definition and shared row/version guards
release before callbacks. Pending-change walks retain original byte owners
through heap relocation. Inherited targets map query columns to stored columns
and enforce each physical table's constraints.

Validation and maintenance use current committed and earlier own changes while
preserving ordinary transaction query snapshots. Their row buffers remain
bounded and are reused. The [implementation guide](docs/implemented-baseline.md)
owns the detailed contracts, completed work, and limits.

Still to do:

- Complete reader protection for authoritative metadata walks and remaining
  executor reads that retain only a version stamp. Sequential SQL byte scans
  and exports already keep selected versions and their source files. Metadata
  walks still consult live row state; coordinate their updates and retirement.
- Extend partition attachment and all write paths to reordered physical
  columns; keep the current explicit rejection until those paths agree.
- Finish shared-state protection for table creation/removal and engine-wide
  publication.
- Integrate bounded heap relocation with concurrent table publication and
  maintenance scheduling. Complete rewrites must preserve historical object
  versions needed by active readers.
- Verify release after errors, rollback, and transaction end, and prevent
  reused identities from being mistaken for older data.

Done when: an unrelated write can complete while a reader retains old data;
maintenance preserves that data; failed publication leaves the prior state
valid; all reader reservations are counted at startup; exhaustion, identity
reuse, rollback, and recovery are tested without runtime allocation.

### 2. Let storage requests and maintenance overlap

**User example:** one customer reads an uncached invoice while another reads a
cached invoice. The second request should not wait for the first download.
A checkpoint or storage merge should allow ordinary queries to keep progressing.

- Reserve independent request slots and scratch buffers at startup. Keep cache
  checks and updates brief; do not hold broad locks during slow object I/O.
- Coordinate requests for the same missing block, allow bounded parallel reads
  and prefetch, and release requests correctly on cancellation.
- Build checkpoint and merge output separately, then publish the new object
  references in a short, consistent update.
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

SQL, maintenance, and recovery share checked row and transaction identity
counters. Creating a transaction reserves its identity and active-registry entry
together; full capacities fail explicitly and leave existing owners intact.

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
configurable startup reservations. A change to durable identities requires the
[format migration procedure](docs/durable-format.md).

Keep test dependencies reproducible and verify their installation before running
comparisons. Fix interrupted downloads with bounded retries and verified caches;
do not extend test timeouts or accept partial setup.

The roadmap is finished when the steps above pass their stated checks and the
supported SQL, formats, monitoring, credentials, packages, backup/restore, and
replacement remain verified together. Keep current behavior in its
[owning guide](docs/README.md), unfinished work here, and dated observations
with their original [evidence](benchmarks/README.md).
