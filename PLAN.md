# pos3ql roadmap

Reviewed after PR #599 on 2026-10-06, against
`708d5b475845492bdaa29dd573a6f508770acc62`.

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
row state, statistics, serial state, and physical maintenance. Readers must keep
consistent definition images without copying a wide definition for each row.
Publication, rollback, slot retirement, template cloning, and recovery must use
one coherent lifecycle and release guards before nested catalog resolution.

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
