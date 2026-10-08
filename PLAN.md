# pos3ql roadmap

Reviewed after PR #603 on 2026-10-07, against
`9c2465cb261755aa375a6fd837ee9dd582fdf04a`.

[Architecture](README.md) · [Current capabilities](docs/implemented-baseline.md) ·
[Compatibility](docs/postgresql-18-compatibility.md) · [Contributing](CONTRIBUTING.md)

## Destination

Deliver a PostgreSQL-compatible, object-native database with fixed startup
memory, useful concurrent execution, recoverable durable publication, and
published performance evidence for representative deployments.

SQL, wire, catalog, tool, and logical-replication behavior are compatibility
boundaries. Object storage is authoritative in durable mode. One direct
S3-compatible implementation serves every qualified provider. Unsupported
behavior fails explicitly; runtime pools never grow to rescue an operation.

PostgreSQL heap pages, physical XLOG/streaming replication, binary-WAL tools,
server ABI/hooks, and third-party extension certification are non-goals.
The existing SQL-extension package lifecycle remains accepted behavior.
Active-active writers and transparent shared-storage replicas are not part of
the current single-writer protocol.

## Where we are

| Area | Current state | Remaining qualification or implementation |
|---|---|---|
| SQL and clients | Broad PostgreSQL 18 SQL, catalogs, types, procedures, wire, drivers, and dump/restore; explicit command disposition | Continue differential discovery and accepted-capacity coverage; documented scale divergences remain |
| Object-native storage | Commit barriers, checkpoints, warm/cold caches, physical indexes, paced maintenance, recovery and fault simulation | Preserve these guarantees through concurrency and representative load |
| Durable operations | Writer fencing, named backups, independent-prefix export, point-in-time recovery, probes, credential rotation, release packages, passive-candidate promotion | Representative multi-host recovery and failover evidence with real routing and independent object storage |
| Concurrency | Worker-private statement state and synchronized catalog/metadata boundaries | Tables, rows, engine publication, fixed workers, and one-through-N scaling |
| Performance | Reproducible fixture/MinIO/SeaweedFS suite and two vanilla PostgreSQL controls; retained exploratory runs | Pinned hardware, independent object storage, long runs, repeatability, and published results |

Implemented operations have recovery and external test fixtures. Their existence
is not proof that the production topology or its performance is complete.
[Current behavior](docs/implemented-baseline.md) and the
[measurement contract](docs/performance.md) provide the detailed boundaries.

## Remaining sequence

### 1. Table definitions and row mutation

Synchronize table identity, ownership, transaction-visible definition versions,
row state, statistics, and physical maintenance. Readers must keep
consistent definition images without copying a wide definition for each row.
Publication, rollback, slot retirement, template cloning, and recovery must use
one coherent lifecycle and release guards before nested catalog resolution.

Table-owned serial positions now have a per-table synchronization boundary.
Counter advances, transactional resets, replay, checkpoint reads, template
cloning, and slot reuse use the same state API. WAL captures one consistent
image; acknowledgement clears dirty state only for unchanged positions staged
by that transaction. Integer overflow leaves the prior position intact.
This prepares serial state for overlapping execution; definitions, rows,
statistics, physical maintenance, and engine publication remain exclusive.
Default assignment borrows its visible definition instead of copying the full
maximum-width image for each generated value.

INSERT, UPDATE, DELETE, MERGE, and COPY retain immutable table definitions across
mutable callbacks. UPDATE and MERGE capture each physical table once per
statement instead of copying the maximum-width definition per row. Reader
owners release their startup-reserved cells when execution returns, including
errors; retained images survive definition rollback, publication, and table
slot reuse. Reacquiring a reused identity fails with a serialization error.
Dense occupancy flags avoid touching unused image pages at startup.
The global image capacity is table slots × query workspace slots ×
(maximum catalog versions per object + 1); exhaustion is a named program limit.
COPY retains its owner across data messages; DDL event triggers retain their
pre-change image without consuming statement arena space. Nested-trigger pool
exhaustion rolls back the outer mutation and releases capacity for retry.
Relation row maps now use a per-table read boundary. Point reads and iteration
return detached row-state images; full walks and checkpoint batches retain a
consistent map view. Row mutation, recovery, rollback, template cloning, and
slot reuse require exclusive map access. Table startup accounting includes the
lock controls, and exhaustion preserves existing rows.
Pending and committed version arrays now share one guarded owner with their
free lists. Each array owns its free-list control, so allocator calls cannot
pair it with another pool. Visibility holds one view across both chains and
releases it before object-store lookup. Rollback, pruning, publication, removal, and compaction
require exclusive pool ownership; pool controls are charged at startup.
SQL and checkpoint chain readers now require an issued row read that retains
its version owner. Point lookup acquires version ownership before copying map
metadata; full resident walks borrow one combined map/version view. Raw copied
row metadata cannot be supplied to visibility or chain lookup APIs. Immutable
SST reads carry no resident handles. Heap access, statistics, maintenance, and
shared row publication still require mutation boundaries; execution remains serial.

Live definition publication remains exclusive. Query scopes retain definition
references beyond lookup, so their ownership must change before definitions
can move behind shared publication guards. Retained DML images do not close
that query-scope boundary. These changes remain prerequisites for fixed workers.
Forced-spill and reference differential coverage split their overloaded corpus
slices into complementary workers within the unchanged 15-minute ceiling.
The timeout guard verifies complete, disjoint corpus assignment for both
matrices across mixed partition widths. The ordinary PostgreSQL regression
file slice also has complementary workers; upstream ranges stay together and
statement progress is streamed through the outer harness so deadline failures
retain diagnostics. Runner stack samples traced the UUID timestamp/window deadline to copying wide
routine payloads before candidate filtering. Routine lookup now filters compact
transaction-visible identity and kind under the catalog guard before copying
candidates, then releases the guard before nested overload/catalog resolution.
The complete upstream probe remains the PostgreSQL and deadline regression.
[Stack sampling](https://github.com/e6qu/pos3ql/actions/runs/37693402941)
identified the copy path; [focused validation](https://github.com/e6qu/pos3ql/actions/runs/37695506990)
passed the full probe with zero mismatches after filtering candidates.

Completion evidence:

- concurrent readers and writers retain valid transaction-visible images;
- rollback, exhaustion, identity reuse, and failed creation preserve prior state;
- declared pools and retained images are charged exactly at startup;
- cold recovery and PostgreSQL differential behavior remain correct; and
- access-path and request-shape gates detect performance regressions.

### 2. Engine publication and transaction ownership

Make engine-owned prepared transaction slots, WAL staging, transaction/row/LSN
allocation, group publication, and response barriers safe for overlapping
execution. Synchronized prepared catalog metadata is already implemented;
the engine's transaction ownership and publication still require exclusive
execution.

Completion evidence: coherent MVCC and lock ordering, durable acknowledgement,
correct unknown-outcome errors, cancellation, retry, savepoint and two-phase
recovery, and allocation-free publication under concurrent fault injection.

### 3. Fixed execution workers

Replace the reactor's local queue drain with a startup-sized worker set.
Preserve dispatcher lease ownership, FIFO backpressure, connection/database
identity, fairness, cancellation, object-read waits, and publication ordering.
Additional query workspaces currently isolate state; they do not execute queries
in parallel.

Completion evidence: useful one-through-N scaling for read-only, write-heavy,
and mixed workloads, with bounded saturation, fixed memory, unchanged MVCC,
and unchanged durable response barriers. A global engine lock is not completion
of this objective.

### 4. Representative performance and operations

Run the [full measurement suite](docs/performance.md) on pinned hardware with an
independently operated S3-compatible service. The outstanding inputs are the
host, owner-only credential/configuration path, service and network description,
and storage layout. The harness supports this topology; no representative run
has been published.

Keep both stock PostgreSQL 18 controls: host-available and CPU/memory matched.
PostgreSQL retains its normal local durable storage. Record its medium and
durability settings separately from pos3ql's object requests, network, and
cache tiers. Local MinIO and SeaweedFS qualify independent implementations,
not an independently operated deployment or a provider performance ranking.

Publish raw artifacts and reproducible reports for warm RAM, warm disk, empty
local caches, writes, checkpoint/compaction interference, parameterized joins,
large catalogs, one-through-N workers and logical replicas, freshness, recovery,
and multi-host failover with actual routing. Include latency distributions,
throughput, CPU, memory occupancy, physical access paths, and object requests.
Repeat runs before setting timing thresholds or making production claims.

## Continuing compatibility and capacity work

Use the [capacity inventory](docs/postgresql-18-compatibility.md#capacity-boundaries)
to distinguish PostgreSQL limits, durable identities, configured startup pools,
statement memory, and smaller implementation bounds. Verify accepted width,
error behavior, catalogs, WAL, checkpoints, and object-cold recovery together.

Manifest v14 retains 64 constraints per modeled table kind and 64 domain checks.
Those positions also define durable catalog and referential-trigger OIDs.
Widening requires an explicit format/OID migration; it is not a constant-only
change. Other documented subsets, including XPath and planner behavior, remain
explicit boundaries rather than claims of universal PostgreSQL compatibility.

Reader removal or an incompatible writer must ship a verified offline migration
first. No such format retirement is currently enabled. Follow the
[durable-format contract](docs/durable-format.md).

## What changed in our understanding

- Capacity work exposed shared mutable state beyond the execution arena.
  Catalog synchronization is necessary preparation; worker execution remains open.
- Wide definition copies can regress hot paths. Ownership must protect reader
  lifetimes while metadata scans avoid work proportional to unused capacity.
- Faster checkpoint phases and smaller request counts are useful evidence,
  but shared-host timings and different generation shapes do not isolate a
  causal speedup.
- Matching CPU and memory improves the PostgreSQL comparison; each engine still
  has a different persistence and memory-accounting boundary.
- Recovery fixtures and same-host services establish mechanisms, while realistic
  routing, independent storage, and repeated long runs establish deployment claims.

## Completion gates

The roadmap is complete when all of these hold:

- Advertised SQL/wire shapes and accepted configurations have explicit,
  verified boundaries without truncation or post-startup allocation.
- Format compatibility, monitoring, credentials, packaging, backup/restore,
  and replacement remain qualified end to end.
- Concurrent execution scales through the supported worker range while
  preserving MVCC, durability, fixed memory, and backpressure.
- Published representative results substantiate performance, recovery,
  replica freshness, memory, and object-request claims.

Update this file when a gate changes. Completed chronology and original
measurements are indexed in [history](docs/history/README.md) and
[benchmark evidence](benchmarks/README.md), rather than repeated here.
Source and release documentation links are checked by CI; the release carries
the current document tree with compatibility redirects for old operator paths.
