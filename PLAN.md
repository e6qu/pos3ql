# pos3ql roadmap

[Architecture](README.md) · [Current capabilities](docs/implemented-baseline.md) ·
[Compatibility](docs/postgresql-18-compatibility.md) · [Contributing](CONTRIBUTING.md)

## Destination

Deliver a PostgreSQL-compatible, object-native database with fixed startup
memory, overlapping reads and writes, recoverable durable publication, and
published performance evidence for representative deployments.

SQL, wire, catalog, tool, and logical-replication behavior are compatibility
boundaries. Object storage is authoritative in durable mode. One direct
S3-compatible implementation serves every qualified provider. Unsupported
behavior fails explicitly; runtime pools never grow to rescue an operation.

The [compatibility contract](docs/postgresql-18-compatibility.md) owns accepted
behavior and non-goals. One writer incarnation owns each durable prefix; many
transactions within that process must execute concurrently.

## Where we are

| Area | Implemented | Open gate |
|---|---|---|
| Compatibility | Broad PostgreSQL 18 SQL, catalogs, types, procedures, wire, drivers, dump/restore, and explicit command disposition | Preserve accepted shapes and explicit limits through the remaining changes |
| Persistence | Durable response barriers, checkpoints, indexes, caches, paced maintenance, and cold recovery | Preserve these guarantees under concurrent execution and representative load |
| Operations | Writer fencing, backups, independent-prefix export, point-in-time recovery, probes, credential rotation, packages, and passive promotion | Qualify multi-host recovery and failover with real routing and independent storage |
| Concurrency | Private statement workspaces; synchronized catalogs, serial positions, row maps/version readers, retained query/DML definitions, and heap byte ownership | Shared table/row mutation, engine publication, and fixed execution workers |
| Performance | Fixture, MinIO, and SeaweedFS harness with host-available and CPU/memory-matched vanilla PostgreSQL 18 controls | Repeated representative runs and published raw evidence |

Execution remains serial. Guarded reads and retained images are prerequisites;
additional workspaces do not yet execute queries in parallel. Implemented
recovery mechanisms and same-host storage fixtures do not qualify a production
topology. Current contracts belong in the [implemented baseline](docs/implemented-baseline.md)
and its linked documents; completed investigations belong in [history](docs/history/README.md).

## Remaining sequence

### 1. Row publication, reader retention, and reclamation

Allow readers to retain immutable selected images while unrelated writes progress.
Heap appends and pending row publication progress while existing byte readers
retain their images. Preparation precedes short, validated version/map updates;
rollback uses the same ownership. Authoritative row-state walks retain bounded
row identities and release broad metadata ownership before callbacks; their
frozen overlay coverage survives eviction during SST merging. Pending uniqueness
walks release chain ownership before byte reads and waits. Physical outer-join
match tracking uses row identities across scan orders. Committed publication,
table lifecycle, optimized byte/checkpoint walks, and in-place compaction still
require further concurrency work.

- Complete concurrent table lifecycle, committed row publication, snapshot
  registration, and statistics ownership with explicit lock ordering. Replace
  optimized byte/checkpoint walk ownership with bounded retained scan generations.
- Preserve active snapshots and retained table/SST generations through retirement.
  Stale-token detection does not replace retention of a live reader's state.
- Separate reclamation from active readers. Evaluate startup-sized segments and
  bounded pins or reader epochs before enabling concurrent physical maintenance.

Acceptance: a retained reader permits an unrelated write to complete; failed
publication preserves prior state; maintenance retains active images; bounded
exhaustion, identity reuse, rollback, and cold recovery remain allocation-free.

### 2. Concurrent object reads and maintenance

Give queries and maintenance independent, startup-budgeted scratch and request
ownership. Audit the shared cache/object-store stack so slow I/O does not hold
broad ownership that excludes unrelated requests.

- Keep cache lookup/fill ownership brief; coordinate duplicate misses and bounded
  parallel reads, prefetch, and cancellation.
- Build checkpoint and compaction replacements independently of foreground reads
  and writes; publish a coherent generation through a short root transition.
- Budget foreground reads, durable publication, and maintenance fairly. Preserve
  object retention until every active reader and backup releases its generation.

Acceptance: unrelated warm reads and writes progress during a cold miss and
maintenance; concurrent misses use bounded slots; request/byte amplification,
reclamation, and cold recovery remain qualified through the shared S3 contract.

### 3. Transaction ownership and durable publication

Synchronize prepared slots, WAL staging, transaction/row/LSN allocation, commit
ordering, and response barriers. Concurrent statement execution must not hold
whole-engine ownership while waiting for durable publication.

Acceptance: coherent MVCC and lock ordering, grouped durable acknowledgement,
unknown-outcome errors, cancellation, retries, savepoints, and two-phase recovery
under concurrent fault injection without runtime allocation.

### 4. Fixed execution workers

Replace the reactor's local queue drain with a startup-sized worker set.
Preserve dispatcher leases, FIFO backpressure, connection/database identity,
fairness, cancellation, object-read waits, and publication ordering.

Acceptance: useful one-through-N scaling for read-only, write-heavy, and mixed
workloads with bounded saturation, fixed memory, unchanged MVCC, and durable
response barriers. Serializing the whole engine behind a global lock does not
satisfy this gate.

### 5. Representative performance and operations

Run the [measurement suite](docs/performance.md) on pinned hardware with an
independently operated S3-compatible service. Required deployment inputs are
the host, owner-only credential/configuration path, service/network description,
and storage layout. The harness supports this topology; no representative run
has been published.

Retain both stock PostgreSQL 18 controls: host-available and CPU/memory matched,
using normal local durable storage. Record its medium and durability settings
separately from pos3ql's object requests, network, and cache tiers. Same-host
MinIO and SeaweedFS qualify independent implementations; their measurements do
not establish a provider ranking or independently operated deployment.

Acceptance: reproducible raw artifacts and repeated reports for warm RAM,
warm disk, empty caches, writes, checkpoint/compaction interference, parameterized
joins, large catalogs, one-through-N workers and logical replicas, freshness,
recovery, and multi-host failover with actual routing. Include latency
distributions, throughput, CPU, memory occupancy, physical access paths, and
object requests. Evaluate physical ordering, packing, projection, pruning, and
compaction through request, byte, and rewrite amplification alongside latency.
Set timing thresholds only after repeatable measurements.

## Continuing compatibility and capacity work

The [capacity inventory](docs/postgresql-18-compatibility.md#capacity-boundaries)
separates PostgreSQL limits, durable identities, startup pools, statement memory,
and smaller implementation bounds. Verify accepted widths, errors, catalogs,
WAL, checkpoints, and cold recovery together throughout the sequence.

Capacity widening that changes durable identities follows the
[format migration contract](docs/durable-format.md). Preserve accepted widths
and explicit compatibility subsets through every concurrency change.

## Completion gates

The roadmap is complete when all of these hold:

- Advertised SQL/wire shapes and accepted configurations have verified, explicit
  boundaries without truncation or post-startup allocation.
- Formats, monitoring, credentials, packages, backup/restore, and replacement
  remain qualified end to end.
- Concurrent execution scales through the supported worker range; reads, writes,
  cold misses, and maintenance overlap while preserving MVCC, durability, fixed
  memory, and backpressure.
- Published representative results substantiate performance, recovery,
  replica freshness, memory, and object-request claims.

Update this file when an open gate or acceptance criterion changes. Keep current
behavior in its owning contract and past observations with their original
[evidence](benchmarks/README.md), rather than appending completed-work journals.
