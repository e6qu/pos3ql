# pos3ql roadmap

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
Active-active writers and transparent shared-storage replicas are outside the
current single-writer protocol.

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

### 1. Table definitions and row mutation

Complete shared ownership before enabling overlapping execution:

- Coordinate definition publication with table identity and existence. Committed
  definitions, pending heads, and version slots have guarded ownership; query and
  DML readers retain immutable images. Lifecycle coordination remains exclusive.
- Synchronize row publication, statistics, and physical maintenance through a
  coherent table lifecycle, including rollback, retirement, cloning, and recovery.
- Pin heap locations across visibility lookup and concurrent relocation. Heap
  generations reject detached locations after relocation, including reused ranges;
  byte guards and rejection do not preserve a locator across visibility lookup.
- Establish lock ordering and release guards before nested catalog resolution.
  Reuse retained images and compact metadata instead of copying wide definitions
  per row. Long-lived heap images consume the fixed statement arena.

Acceptance: concurrent readers/writers retain valid images; exhaustion,
rollback, identity reuse, and failed creation preserve prior state; all controls
and retained capacity are charged at startup; allocation, cold-recovery,
PostgreSQL differential, access-path, and request-shape gates pass.

### 2. Engine publication and transaction ownership

Synchronize engine-owned prepared slots, WAL staging, transaction/row/LSN
allocation, group publication, and response barriers. Prepared catalog metadata
has synchronized ownership; engine execution still requires exclusive access.

Acceptance: coherent MVCC and lock ordering, durable acknowledgement, correct
unknown-outcome errors, cancellation, retries, savepoints, and two-phase recovery
under concurrent fault injection without runtime allocation.

### 3. Fixed execution workers

Replace the reactor's local queue drain with a startup-sized worker set.
Preserve dispatcher leases, FIFO backpressure, connection/database identity,
fairness, cancellation, object-read waits, and publication ordering.

Acceptance: useful one-through-N scaling for read-only, write-heavy, and mixed
workloads with bounded saturation, fixed memory, unchanged MVCC, and durable
response barriers. Serializing the whole engine behind a global lock does not
satisfy this gate.

### 4. Representative performance and operations

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
object requests. Set timing thresholds only after repeatable measurements.

## Continuing compatibility and capacity work

The [capacity inventory](docs/postgresql-18-compatibility.md#capacity-boundaries)
separates PostgreSQL limits, durable identities, startup pools, statement memory,
and smaller implementation bounds. Verify accepted widths, errors, catalogs,
WAL, checkpoints, and cold recovery together throughout the sequence.

Manifest v14's 64 constraint/domain-check positions also define durable catalog
and referential-trigger OIDs. Widening requires an explicit format/OID migration.
Documented subsets, including XPath and planner behavior, remain explicit
boundaries. Incompatible writers or reader removal require a verified offline
migration under the [durable-format contract](docs/durable-format.md); no format
retirement is currently enabled.

## Completion gates

The roadmap is complete when all of these hold:

- Advertised SQL/wire shapes and accepted configurations have verified, explicit
  boundaries without truncation or post-startup allocation.
- Formats, monitoring, credentials, packages, backup/restore, and replacement
  remain qualified end to end.
- Concurrent execution scales through the supported worker range while
  preserving MVCC, durability, fixed memory, and backpressure.
- Published representative results substantiate performance, recovery,
  replica freshness, memory, and object-request claims.

Update this file when an open gate or acceptance criterion changes. Keep current
behavior in its owning contract and past observations with their original
[evidence](benchmarks/README.md), rather than appending completed-work journals.
